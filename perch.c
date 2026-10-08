#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <gdk-pixbuf/gdk-pixbuf.h>
#include <limits.h>
#include <linux/input-event-codes.h>
#include <math.h>
#include <poll.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>
#include <wayland-client.h>

#include "viewporter-client-protocol.h"
#include "xdg-output-unstable-v1-client-protocol.h"
#include "xdg-shell-client-protocol.h"

#define ARRAY_LEN(a) (sizeof(a) / sizeof *(a))
#define IPC_MAGIC 0x50455243u
#define MAX_PATHS 4096u
#define MAX_PATH_SIZE PATH_MAX
#define MAX_CLIENTS 16

struct app;

struct client {
    int fd;
    int header_received;
    uint32_t path_count;
    uint32_t paths_received;
    char **paths;
    GdkPixbufLoader *loader;
};

struct output {
    struct output *next;
    struct wl_output *wl;
    struct zxdg_output_v1 *xdg;
    uint32_t name;
    int pixel_width;
    int pixel_height;
    int logical_width;
    int logical_height;
    int scale;
    int transform;
};

struct pin {
    struct pin *next;
    struct app *app;
    struct wl_surface *surface;
    struct xdg_surface *xdg_surface;
    struct xdg_toplevel *toplevel;
    struct wp_viewport *viewport;
    struct wl_buffer *buffer;
    double zoom;
    int image_width;
    int image_height;
    int presented;
    int dirty;
    int width;
    int height;
};

struct app {
    struct wl_display *display;
    struct wl_registry *registry;
    struct wl_compositor *compositor;
    struct wl_shm *shm;
    struct wl_seat *seat;
    struct wl_pointer *pointer;
    struct xdg_wm_base *wm_base;
    struct wp_viewporter *viewporter;
    struct zxdg_output_manager_v1 *output_manager;
    struct output *outputs;
    struct pin *pins;
    struct pin *pointer_pin;
    uint32_t shm_format;
    int server_fd;
};

static void render_pin(struct pin *pin);
static void destroy_pin(struct pin *pin);

static socklen_t socket_address(struct sockaddr_un *address)
{
    char default_name[32];
    const char *name = getenv("PERCH_SOCKET");
    if (!name || !*name) {
        snprintf(default_name, sizeof(default_name), "perch-%u",
                 (unsigned)getuid());
        name = default_name;
    }
    const size_t length = strlen(name);

    if (length >= sizeof(address->sun_path)) {
        fprintf(stderr, "perch: socket name is too long\n");
        exit(EXIT_FAILURE);
    }

    memset(address, 0, sizeof(*address));
    address->sun_family = AF_UNIX;
    memcpy(address->sun_path + 1, name, length);
    return (socklen_t)(offsetof(struct sockaddr_un, sun_path) + length + 1);
}

static int send_packet(int fd, const void *data, size_t size)
{
    ssize_t result;
    do {
        result = send(fd, data, size, MSG_NOSIGNAL);
    } while (result < 0 && errno == EINTR);
    return result >= 0 && (size_t)result == size ? 0 : -1;
}

static ssize_t receive_packet(int fd, void *data, size_t size)
{
    ssize_t result;
    do {
        result = recv(fd, data, size, MSG_TRUNC);
    } while (result < 0 && errno == EINTR);
    return result;
}

static int create_shm_file(size_t size)
{
    int fd = memfd_create("perch", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (fd < 0 || ftruncate(fd, (off_t)size) < 0) {
        if (fd >= 0) {
            close(fd);
        }
        return -1;
    }
    fcntl(fd, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL);
    return fd;
}

static void copy_pixbuf(void *destination, GdkPixbuf *pixbuf, uint32_t format)
{
    const int width = gdk_pixbuf_get_width(pixbuf);
    const int height = gdk_pixbuf_get_height(pixbuf);
    const int channels = gdk_pixbuf_get_n_channels(pixbuf);
    const int source_stride = gdk_pixbuf_get_rowstride(pixbuf);
    const guchar *source = gdk_pixbuf_read_pixels(pixbuf);
    uint8_t *target = destination;
    const int red = format == WL_SHM_FORMAT_ABGR8888 ? 0 : 2;
    const int blue = 2 - red;

    for (int y = 0; y < height; ++y) {
        const guchar *row = source + (size_t)y * (size_t)source_stride;
        int x = 0;
        if (format == WL_SHM_FORMAT_ABGR8888 && channels == 4) {
            while (x < width && row[(size_t)x * 4 + 3] == 255) {
                ++x;
            }
            if (x == width) {
                memcpy(target, row, (size_t)width * 4);
                target += (size_t)width * 4;
                continue;
            }
        }
        for (x = 0; x < width; ++x) {
            const guchar *pixel = row + (size_t)x * (size_t)channels;
            const uint8_t alpha = channels == 4 ? pixel[3] : 255;
            if (alpha == 255) {
                target[red] = pixel[0];
                target[1] = pixel[1];
                target[blue] = pixel[2];
            } else {
                target[red] = (uint8_t)((pixel[0] * alpha + 127) / 255);
                target[1] = (uint8_t)((pixel[1] * alpha + 127) / 255);
                target[blue] = (uint8_t)((pixel[2] * alpha + 127) / 255);
            }
            target[3] = alpha;
            target += 4;
        }
    }
}

static struct wl_buffer *create_buffer(struct app *app, GdkPixbuf *pixbuf)
{
    const int width = gdk_pixbuf_get_width(pixbuf);
    const int height = gdk_pixbuf_get_height(pixbuf);
    if (width <= 0 || height <= 0 || width > INT_MAX / 4) {
        return NULL;
    }
    const int stride = width * 4;
    const size_t size = (size_t)stride * (size_t)height;
    if (size > INT_MAX) {
        return NULL;
    }

    const int fd = create_shm_file(size);
    if (fd < 0) {
        return NULL;
    }
    void *mapping = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    struct wl_shm_pool *pool = mapping == MAP_FAILED ? NULL :
        wl_shm_create_pool(app->shm, fd, (int)size);
    struct wl_buffer *buffer = pool ? wl_shm_pool_create_buffer(
        pool, 0, width, height, stride, app->shm_format) : NULL;
    if (pool) {
        wl_shm_pool_destroy(pool);
    }
    close(fd);
    if (mapping != MAP_FAILED) {
        if (buffer) {
            copy_pixbuf(mapping, pixbuf, app->shm_format);
        }
        munmap(mapping, size);
    }
    return buffer;
}

static void output_bounds(struct app *app, int *width, int *height)
{
    *width = 1920;
    *height = 1080;
    int found = 0;

    for (const struct output *output = app->outputs; output; output = output->next) {
        int w = output->logical_width;
        int h = output->logical_height;
        if (w <= 0 || h <= 0) {
            const int scale = output->scale > 0 ? output->scale : 1;
            w = output->pixel_width / scale;
            h = output->pixel_height / scale;
            if (output->transform & 1) {
                const int swap = w;
                w = h;
                h = swap;
            }
        }
        if (w <= 0 || h <= 0) {
            continue;
        }
        if (!found || w < *width) {
            *width = w;
        }
        if (!found || h < *height) {
            *height = h;
        }
        found = 1;
    }
}

static void update_pin_size(struct pin *pin)
{
    pin->width = (int)fmin(INT_MAX,
                          fmax(1.0, round(pin->image_width * pin->zoom)));
    pin->height = (int)fmin(INT_MAX,
                           fmax(1.0, round(pin->image_height * pin->zoom)));
    pin->dirty = 1;
    xdg_toplevel_set_min_size(pin->toplevel, pin->width, pin->height);
    xdg_toplevel_set_max_size(pin->toplevel, pin->width, pin->height);
}

static void render_pin(struct pin *pin)
{
    if (!pin->dirty) {
        return;
    }
    wp_viewport_set_destination(pin->viewport, pin->width, pin->height);
    if (!pin->presented) {
        wl_surface_attach(pin->surface, pin->buffer, 0, 0);
        wl_surface_damage_buffer(
            pin->surface, 0, 0, pin->image_width, pin->image_height);
        pin->presented = 1;
    }
    wl_surface_commit(pin->surface);
    pin->dirty = 0;
}

static void xdg_surface_configure(
    void *data, struct xdg_surface *surface, uint32_t serial)
{
    struct pin *pin = data;
    xdg_surface_ack_configure(surface, serial);
    render_pin(pin);
}

static const struct xdg_surface_listener xdg_surface_listener = {
    .configure = xdg_surface_configure,
};

static void toplevel_configure(void *data, struct xdg_toplevel *toplevel,
                               int32_t width, int32_t height,
                               struct wl_array *states)
{
}

static void toplevel_close(void *data, struct xdg_toplevel *toplevel)
{
    destroy_pin(data);
}

static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure,
    .close = toplevel_close,
};

static struct pin *create_pin(struct app *app, GdkPixbuf *loaded,
                              const char *name)
{
    GdkPixbuf *image = gdk_pixbuf_apply_embedded_orientation(loaded);
    if (!image) {
        fprintf(stderr, "perch: cannot orient %s\n", name);
        return NULL;
    }

    struct pin *pin = calloc(1, sizeof(*pin));
    if (!pin) {
        g_object_unref(image);
        return NULL;
    }
    pin->app = app;
    pin->image_width = gdk_pixbuf_get_width(image);
    pin->image_height = gdk_pixbuf_get_height(image);
    pin->buffer = create_buffer(app, image);
    if (!pin->buffer) {
        fprintf(stderr, "perch: unable to allocate image buffer\n");
        g_object_unref(image);
        free(pin);
        return NULL;
    }
    g_object_unref(image);

    int output_width;
    int output_height;
    output_bounds(app, &output_width, &output_height);
    const double width_scale = output_width * 0.9 / pin->image_width;
    const double height_scale = output_height * 0.9 / pin->image_height;
    pin->zoom = fmin(1.0, fmin(width_scale, height_scale));

    pin->surface = wl_compositor_create_surface(app->compositor);
    if (pin->surface) {
        pin->xdg_surface = xdg_wm_base_get_xdg_surface(
            app->wm_base, pin->surface);
    }
    if (pin->xdg_surface) {
        pin->toplevel = xdg_surface_get_toplevel(pin->xdg_surface);
    }
    if (pin->surface) {
        pin->viewport = wp_viewporter_get_viewport(
            app->viewporter, pin->surface);
    }
    if (!pin->surface || !pin->xdg_surface || !pin->toplevel || !pin->viewport) {
        destroy_pin(pin);
        return NULL;
    }

    wl_surface_set_user_data(pin->surface, pin);
    xdg_surface_add_listener(pin->xdg_surface, &xdg_surface_listener, pin);
    xdg_toplevel_add_listener(pin->toplevel, &toplevel_listener, pin);
    xdg_toplevel_set_title(pin->toplevel, "Perch");
    xdg_toplevel_set_app_id(pin->toplevel, "perch");
    update_pin_size(pin);

    pin->next = app->pins;
    app->pins = pin;
    wl_surface_commit(pin->surface);
    return pin;
}

static struct pin *open_pin(struct app *app, const char *path)
{
    GError *error = NULL;
    const int fd = open(path, O_RDONLY | O_CLOEXEC | O_NONBLOCK);
    struct stat metadata;
    if (fd < 0 || fstat(fd, &metadata) < 0 || !S_ISREG(metadata.st_mode)) {
        fprintf(stderr, "perch: cannot open %s: expected a regular image file\n",
                path);
        if (fd >= 0) {
            close(fd);
        }
        return NULL;
    }
    char fd_path[64];
    snprintf(fd_path, sizeof(fd_path), "/proc/self/fd/%d", fd);
    GdkPixbuf *loaded = gdk_pixbuf_new_from_file(fd_path, &error);
    close(fd);
    if (!loaded) {
        fprintf(stderr, "perch: cannot open %s: %s\n", path,
                error ? error->message : "unsupported image");
        g_clear_error(&error);
        return NULL;
    }

    struct pin *pin = create_pin(app, loaded, path);
    g_object_unref(loaded);
    return pin;
}

static void destroy_pin(struct pin *pin)
{
    if (!pin) {
        return;
    }
    struct app *app = pin->app;
    struct pin **link = &app->pins;
    while (*link && *link != pin) {
        link = &(*link)->next;
    }
    if (*link) {
        *link = pin->next;
    }
    if (app->pointer_pin == pin) {
        app->pointer_pin = NULL;
    }
    if (pin->viewport) {
        wp_viewport_destroy(pin->viewport);
    }
    if (pin->toplevel) {
        xdg_toplevel_destroy(pin->toplevel);
    }
    if (pin->xdg_surface) {
        xdg_surface_destroy(pin->xdg_surface);
    }
    if (pin->surface) {
        wl_surface_destroy(pin->surface);
    }
    if (pin->buffer) {
        wl_buffer_destroy(pin->buffer);
    }
    free(pin);
}

static void pointer_enter(void *data, struct wl_pointer *pointer, uint32_t serial,
                          struct wl_surface *surface, wl_fixed_t x, wl_fixed_t y)
{
    struct app *app = data;
    app->pointer_pin = wl_surface_get_user_data(surface);
}

static void pointer_leave(void *data, struct wl_pointer *pointer, uint32_t serial,
                          struct wl_surface *surface)
{
    struct app *app = data;
    if (app->pointer_pin && app->pointer_pin->dirty) {
        render_pin(app->pointer_pin);
    }
    app->pointer_pin = NULL;
}

static void pointer_motion(void *data, struct wl_pointer *pointer, uint32_t time,
                           wl_fixed_t x, wl_fixed_t y)
{
}

static void pointer_button(void *data, struct wl_pointer *pointer, uint32_t serial,
                           uint32_t time, uint32_t button, uint32_t state)
{
    struct app *app = data;
    struct pin *pin = app->pointer_pin;
    if (!pin || state != WL_POINTER_BUTTON_STATE_PRESSED) {
        return;
    }
    if (button == BTN_LEFT) {
        xdg_toplevel_move(pin->toplevel, app->seat, serial);
    } else if (button == BTN_RIGHT) {
        destroy_pin(pin);
    }
}

static void pointer_axis(void *data, struct wl_pointer *pointer, uint32_t time,
                         uint32_t axis, wl_fixed_t value)
{
    struct app *app = data;
    struct pin *pin = app->pointer_pin;
    if (!pin || axis != WL_POINTER_AXIS_VERTICAL_SCROLL) {
        return;
    }

    const double steps = -wl_fixed_to_double(value) / 15.0;
    pin->zoom = fmin(8.0, fmax(0.05, pin->zoom * pow(1.1, steps)));
    update_pin_size(pin);
}

static void pointer_frame(void *data, struct wl_pointer *pointer)
{
    struct pin *pin = ((struct app *)data)->pointer_pin;
    if (pin && pin->dirty) {
        render_pin(pin);
    }
}

static void pointer_axis_source(void *data, struct wl_pointer *pointer,
                                uint32_t source)
{
}

static void pointer_axis_stop(void *data, struct wl_pointer *pointer,
                              uint32_t time, uint32_t axis)
{
}

static void pointer_axis_discrete(void *data, struct wl_pointer *pointer,
                                  uint32_t axis, int32_t discrete)
{
}

static const struct wl_pointer_listener pointer_listener = {
    .enter = pointer_enter,
    .leave = pointer_leave,
    .motion = pointer_motion,
    .button = pointer_button,
    .axis = pointer_axis,
    .frame = pointer_frame,
    .axis_source = pointer_axis_source,
    .axis_stop = pointer_axis_stop,
    .axis_discrete = pointer_axis_discrete,
};

static void seat_capabilities(void *data, struct wl_seat *seat,
                              uint32_t capabilities)
{
    struct app *app = data;
    if ((capabilities & WL_SEAT_CAPABILITY_POINTER) && !app->pointer) {
        app->pointer = wl_seat_get_pointer(seat);
        wl_pointer_add_listener(app->pointer, &pointer_listener, app);
    } else if (!(capabilities & WL_SEAT_CAPABILITY_POINTER) && app->pointer) {
        wl_pointer_release(app->pointer);
        app->pointer = NULL;
    }
}

static void seat_name(void *data, struct wl_seat *seat, const char *name)
{
}

static const struct wl_seat_listener seat_listener = {
    .capabilities = seat_capabilities,
    .name = seat_name,
};

static void shm_format(void *data, struct wl_shm *shm, uint32_t format)
{
    if (format == WL_SHM_FORMAT_ABGR8888) {
        ((struct app *)data)->shm_format = format;
    }
}

static const struct wl_shm_listener shm_listener = {
    .format = shm_format,
};

static void wm_base_ping(void *data, struct xdg_wm_base *wm_base, uint32_t serial)
{
    xdg_wm_base_pong(wm_base, serial);
}

static const struct xdg_wm_base_listener wm_base_listener = {
    .ping = wm_base_ping,
};

static void output_geometry(void *data, struct wl_output *wl_output, int32_t x,
                            int32_t y, int32_t physical_width,
                            int32_t physical_height, int32_t subpixel,
                            const char *make, const char *model, int32_t transform)
{
    ((struct output *)data)->transform = transform;
}

static void output_mode(void *data, struct wl_output *wl_output, uint32_t flags,
                        int32_t width, int32_t height, int32_t refresh)
{
    struct output *output = data;
    if (flags & WL_OUTPUT_MODE_CURRENT) {
        output->pixel_width = width;
        output->pixel_height = height;
    }
}

static void output_done(void *data, struct wl_output *wl_output)
{
}

static void output_scale(void *data, struct wl_output *wl_output, int32_t factor)
{
    ((struct output *)data)->scale = factor;
}

static const struct wl_output_listener output_listener = {
    .geometry = output_geometry,
    .mode = output_mode,
    .done = output_done,
    .scale = output_scale,
};

static void xdg_output_logical_position(void *data, struct zxdg_output_v1 *xdg,
                                        int32_t x, int32_t y)
{
}

static void xdg_output_logical_size(void *data, struct zxdg_output_v1 *xdg,
                                    int32_t width, int32_t height)
{
    struct output *output = data;
    output->logical_width = width;
    output->logical_height = height;
}

static void xdg_output_done(void *data, struct zxdg_output_v1 *xdg)
{
}

static const struct zxdg_output_v1_listener xdg_output_listener = {
    .logical_position = xdg_output_logical_position,
    .logical_size = xdg_output_logical_size,
    .done = xdg_output_done,
};

static void attach_xdg_outputs(struct app *app)
{
    if (!app->output_manager) {
        return;
    }
    for (struct output *output = app->outputs; output; output = output->next) {
        if (!output->xdg) {
            output->xdg = zxdg_output_manager_v1_get_xdg_output(
                app->output_manager, output->wl);
            zxdg_output_v1_add_listener(
                output->xdg, &xdg_output_listener, output);
        }
    }
}

static void registry_global(void *data, struct wl_registry *registry,
                            uint32_t name, const char *interface, uint32_t version)
{
    struct app *app = data;
    if (strcmp(interface, wl_compositor_interface.name) == 0) {
        if (version >= WL_SURFACE_DAMAGE_BUFFER_SINCE_VERSION) {
            app->compositor = wl_registry_bind(
                registry, name, &wl_compositor_interface,
                WL_SURFACE_DAMAGE_BUFFER_SINCE_VERSION);
        }
    } else if (strcmp(interface, wl_shm_interface.name) == 0) {
        app->shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
        wl_shm_add_listener(app->shm, &shm_listener, app);
    } else if (strcmp(interface, wl_seat_interface.name) == 0 && version >= 5) {
        app->seat = wl_registry_bind(registry, name, &wl_seat_interface, 5);
        wl_seat_add_listener(app->seat, &seat_listener, app);
    } else if (strcmp(interface, xdg_wm_base_interface.name) == 0) {
        app->wm_base = wl_registry_bind(
            registry, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(app->wm_base, &wm_base_listener, app);
    } else if (strcmp(interface, wp_viewporter_interface.name) == 0) {
        app->viewporter = wl_registry_bind(
            registry, name, &wp_viewporter_interface, 1);
    } else if (strcmp(interface, zxdg_output_manager_v1_interface.name) == 0) {
        app->output_manager = wl_registry_bind(
            registry, name, &zxdg_output_manager_v1_interface, 1);
        attach_xdg_outputs(app);
    } else if (strcmp(interface, wl_output_interface.name) == 0) {
        struct output *output = calloc(1, sizeof(*output));
        if (!output) {
            return;
        }
        output->name = name;
        output->scale = 1;
        output->wl = wl_registry_bind(
            registry, name, &wl_output_interface, version < 2 ? version : 2);
        output->next = app->outputs;
        app->outputs = output;
        wl_output_add_listener(output->wl, &output_listener, output);
        attach_xdg_outputs(app);
    }
}

static void registry_global_remove(void *data, struct wl_registry *registry,
                                   uint32_t name)
{
    struct output **link = &((struct app *)data)->outputs;
    while (*link && (*link)->name != name) {
        link = &(*link)->next;
    }
    if (*link) {
        struct output *output = *link;
        *link = output->next;
        if (output->xdg) {
            zxdg_output_v1_destroy(output->xdg);
        }
        wl_output_destroy(output->wl);
        free(output);
    }
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_global_remove,
};

static void close_client(struct client *client)
{
    close(client->fd);
    for (uint32_t index = 0; index < client->paths_received; ++index) {
        free(client->paths[index]);
    }
    free(client->paths);
    if (client->loader) {
        gdk_pixbuf_loader_close(client->loader, NULL);
        g_object_unref(client->loader);
    }
    *client = (struct client){ .fd = -1 };
}

static int read_client(struct app *app, struct client *client)
{
    if (!client->header_received) {
        uint32_t header[2];
        if (receive_packet(client->fd, header, sizeof(header)) !=
                (ssize_t)sizeof(header) ||
            header[0] != IPC_MAGIC || header[1] > MAX_PATHS) {
            return -1;
        }
        client->path_count = header[1];
        if (client->path_count == 0) {
            client->loader = gdk_pixbuf_loader_new();
        } else {
            client->paths = calloc(client->path_count, sizeof(*client->paths));
            if (!client->paths) {
                return -1;
            }
        }
        client->header_received = 1;
        return 0;
    }

    if (client->loader) {
        uint8_t packet[4097];
        const ssize_t length = receive_packet(client->fd, packet, sizeof(packet));
        if (length <= 0 || (size_t)length > sizeof(packet)) {
            return -1;
        }
        GError *error = NULL;
        int success;
        if (packet[0] == 1 && length > 1) {
            success = gdk_pixbuf_loader_write(
                client->loader, packet + 1, (gsize)length - 1, &error);
        } else if (packet[0] == 0 && length == 1) {
            success = gdk_pixbuf_loader_close(client->loader, &error);
        } else {
            return -1;
        }
        if (!success) {
            fprintf(stderr, "perch: cannot open stdin: %s\n",
                    error ? error->message : "unsupported image");
            g_clear_error(&error);
            return -1;
        }
        if (packet[0] == 1) {
            return 0;
        }
        GdkPixbuf *loaded = gdk_pixbuf_loader_get_pixbuf(client->loader);
        return loaded && create_pin(app, loaded, "stdin") ? 1 : -1;
    }

    char path[MAX_PATH_SIZE + 1];
    const ssize_t length = receive_packet(client->fd, path, sizeof(path));
    if (length <= 1 || (size_t)length > sizeof(path) ||
        path[length - 1] != '\0' || memchr(path, '\0', (size_t)length - 1)) {
        return -1;
    }
    char *copy = strdup(path);
    if (!copy) {
        return -1;
    }
    client->paths[client->paths_received++] = copy;
    if (client->paths_received < client->path_count) {
        return 0;
    }

    struct pin *const previous_pins = app->pins;
    for (uint32_t index = 0; index < client->path_count; ++index) {
        if (!open_pin(app, client->paths[index])) {
            while (app->pins != previous_pins) {
                destroy_pin(app->pins);
            }
            return -1;
        }
    }
    return 1;
}

static int create_server(void)
{
    const int fd = socket(
        AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    struct sockaddr_un address;
    if (fd < 0 || bind(fd, (struct sockaddr *)&address,
                       socket_address(&address)) < 0 ||
        listen(fd, MAX_CLIENTS) < 0) {
        if (fd >= 0) {
            close(fd);
        }
        return -1;
    }
    return fd;
}

static int client_is_local(int fd)
{
    struct ucred credentials;
    socklen_t size = sizeof(credentials);
    return getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &credentials, &size) == 0 &&
           size == sizeof(credentials) && credentials.uid == getuid();
}

static int run_daemon(void)
{
    setenv("GDK_PIXBUF_MODULE_FILE", PERCH_PIXBUF_MODULE_FILE, 1);
    struct app app = {
        .shm_format = WL_SHM_FORMAT_ARGB8888,
        .server_fd = -1,
    };
    app.display = wl_display_connect(NULL);
    if (!app.display) {
        fprintf(stderr, "perch: cannot connect to Wayland\n");
        return EXIT_FAILURE;
    }

    app.registry = wl_display_get_registry(app.display);
    wl_registry_add_listener(app.registry, &registry_listener, &app);
    wl_display_roundtrip(app.display);
    attach_xdg_outputs(&app);
    wl_display_roundtrip(app.display);

    if (!app.compositor || !app.shm || !app.seat || !app.wm_base ||
        !app.viewporter) {
        fprintf(stderr, "perch: required Wayland protocols are unavailable\n");
        return EXIT_FAILURE;
    }

    app.server_fd = create_server();
    if (app.server_fd < 0) {
        fprintf(stderr, "perch: cannot create daemon socket: %s\n",
                strerror(errno));
        return EXIT_FAILURE;
    }

    struct client clients[MAX_CLIENTS];
    for (size_t index = 0; index < ARRAY_LEN(clients); ++index) {
        clients[index] = (struct client){ .fd = -1 };
    }
    const int display_fd = wl_display_get_fd(app.display);
    for (;;) {
        if (wl_display_dispatch_pending(app.display) < 0) {
            break;
        }
        const int flush_result = wl_display_flush(app.display);
        if (flush_result < 0 && errno != EAGAIN) {
            break;
        }

        size_t client_slot = 0;
        while (client_slot < ARRAY_LEN(clients) && clients[client_slot].fd >= 0) {
            ++client_slot;
        }
        struct pollfd descriptors[2 + MAX_CLIENTS] = {
            {
                .fd = display_fd,
                .events = POLLIN | (flush_result < 0 ? POLLOUT : 0),
            },
            {
                .fd = client_slot < ARRAY_LEN(clients) ? app.server_fd : -1,
                .events = POLLIN,
            },
        };
        for (size_t index = 0; index < ARRAY_LEN(clients); ++index) {
            descriptors[index + 2] = (struct pollfd){
                .fd = clients[index].fd, .events = POLLIN,
            };
        }
        if (poll(descriptors, ARRAY_LEN(descriptors), -1) < 0) {
            if (errno == EINTR) {
                continue;
            }
            break;
        }
        if ((descriptors[0].revents | descriptors[1].revents) &
            (POLLERR | POLLHUP | POLLNVAL)) {
            break;
        }
        if (descriptors[0].revents & POLLIN &&
            wl_display_dispatch(app.display) < 0) {
            break;
        }
        for (size_t index = 0; index < ARRAY_LEN(clients); ++index) {
            const short events = descriptors[index + 2].revents;
            if (!(events & (POLLIN | POLLERR | POLLHUP | POLLNVAL))) {
                continue;
            }
            struct client *client = &clients[index];
            const int status = (events & (POLLERR | POLLNVAL)) ? -1 :
                read_client(&app, client);
            if (status != 0) {
                const uint8_t response = status > 0 ? 0 : 1;
                send_packet(client->fd, &response, sizeof(response));
                close_client(client);
            }
        }
        if (descriptors[1].revents & POLLIN) {
            const int fd = accept4(
                app.server_fd, NULL, NULL, SOCK_CLOEXEC | SOCK_NONBLOCK);
            if (fd >= 0) {
                if (client_is_local(fd)) {
                    clients[client_slot].fd = fd;
                } else {
                    close(fd);
                }
            }
        }
    }

    fprintf(stderr, "perch: Wayland connection closed\n");
    return EXIT_FAILURE;
}

static int run_client(int argc, char **argv)
{
    const int from_stdin = argc == 2 &&
        (strcmp(argv[1], "--stdin") == 0 || strcmp(argv[1], "-") == 0);
    if ((!from_stdin && argc < 2) || (uint32_t)(argc - 1) > MAX_PATHS) {
        fprintf(stderr, "usage: perch [--stdin | IMAGE...]\n");
        return EXIT_FAILURE;
    }

    uint8_t stdin_packet[4097];
    ssize_t stdin_length = 0;
    if (from_stdin) {
        do {
            stdin_length = read(STDIN_FILENO, stdin_packet + 1,
                                sizeof(stdin_packet) - 1);
        } while (stdin_length < 0 && errno == EINTR);
        if (stdin_length < 0) {
            fprintf(stderr, "perch: cannot read stdin: %s\n", strerror(errno));
            return EXIT_FAILURE;
        }
        if (stdin_length == 0) {
            fprintf(stderr, "perch: cannot open stdin: empty stream\n");
            return EXIT_FAILURE;
        }
        stdin_packet[0] = 1;
    }

    const int fd = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    struct sockaddr_un address;
    if (fd < 0 || connect(fd, (struct sockaddr *)&address,
                          socket_address(&address)) < 0) {
        fprintf(stderr, "perch: daemon is unavailable: %s\n", strerror(errno));
        if (fd >= 0) {
            close(fd);
        }
        return EXIT_FAILURE;
    }

    const uint32_t header[] = {
        IPC_MAGIC, from_stdin ? 0 : (uint32_t)(argc - 1),
    };
    if (send_packet(fd, header, sizeof(header)) < 0) {
        close(fd);
        return EXIT_FAILURE;
    }

    if (from_stdin) {
        for (;;) {
            if (send_packet(fd, stdin_packet, (size_t)stdin_length + 1) < 0) {
                close(fd);
                return EXIT_FAILURE;
            }
            if (stdin_length == 0) {
                break;
            }
            do {
                stdin_length = read(STDIN_FILENO, stdin_packet + 1,
                                    sizeof(stdin_packet) - 1);
            } while (stdin_length < 0 && errno == EINTR);
            if (stdin_length < 0) {
                fprintf(stderr, "perch: cannot read stdin: %s\n",
                        strerror(errno));
                close(fd);
                return EXIT_FAILURE;
            }
            stdin_packet[0] = stdin_length == 0 ? 0 : 1;
        }
    }
    for (int index = 1; !from_stdin && index < argc; ++index) {
        char *path = realpath(argv[index], NULL);
        if (!path) {
            fprintf(stderr, "perch: cannot resolve %s: %s\n",
                    argv[index], strerror(errno));
            close(fd);
            return EXIT_FAILURE;
        }
        const size_t size = strlen(path) + 1;
        const int failed = size > MAX_PATH_SIZE + 1 ||
                           send_packet(fd, path, size) < 0;
        free(path);
        if (failed) {
            close(fd);
            return EXIT_FAILURE;
        }
    }

    uint8_t response = 1;
    const ssize_t result = receive_packet(fd, &response, sizeof(response));
    close(fd);
    return result == 1 && response == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
}

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "--daemon") == 0) {
        return run_daemon();
    }
    return run_client(argc, argv);
}
