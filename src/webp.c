#include <webp/decode.h>

int perch_webp_scaled(const uint8_t *input, size_t length, uint8_t *output,
                      size_t size, int width, int height, int stride,
                      int left, int top, int crop_width, int crop_height) {
    WebPDecoderConfig config;
    if (!WebPInitDecoderConfig(&config)) return 0;
    config.output.colorspace = MODE_RGBA;
    config.output.is_external_memory = 1;
    config.output.u.RGBA.rgba = output;
    config.output.u.RGBA.size = size;
    config.output.u.RGBA.stride = stride;
    if (crop_width) {
        config.options.use_cropping = 1;
        config.options.crop_left = left;config.options.crop_top = top;
        config.options.crop_width = crop_width;config.options.crop_height = crop_height;
    }
    config.options.use_scaling = 1;
    config.options.scaled_width = width;
    config.options.scaled_height = height;
    int ok = WebPDecode(input, length, &config) == VP8_STATUS_OK;
    WebPFreeDecBuffer(&config.output);
    return ok;
}

int perch_webp_info(const uint8_t *input, size_t length, int *width, int *height,
                    int *animated) {
    WebPBitstreamFeatures features;
    if (WebPGetFeatures(input, length, &features) != VP8_STATUS_OK) return 0;
    *width = features.width;
    *height = features.height;
    *animated = features.has_animation;
    return 1;
}
