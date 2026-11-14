#include <stdio.h>
#include <setjmp.h>
#include <jpeglib.h>

typedef int (*row_fn)(void *, unsigned, unsigned, const unsigned char *, unsigned);
struct decode_error { struct jpeg_error_mgr base; jmp_buf jump; };
struct decode_progress { struct jpeg_progress_mgr base; row_fn row; void *context; };
static void fail(j_common_ptr jpeg) {
    longjmp(((struct decode_error *)jpeg->err)->jump,1);
}
static void progress(j_common_ptr jpeg) {
    struct decode_progress *p=(struct decode_progress *)jpeg->progress;
    if (!p->row(p->context,0,0,NULL,0)) fail(jpeg);
}
int perch_jpeg_rows(const unsigned char *input, size_t length, unsigned width,
                    unsigned height, unsigned denominator, unsigned left,
                    unsigned top, unsigned right, unsigned bottom,
                    row_fn row, void *context) {
    struct jpeg_decompress_struct jpeg={0};
    struct decode_error error;
    struct decode_progress monitor={0};
    jpeg.err=jpeg_std_error(&error.base);error.base.error_exit=fail;
    if(setjmp(error.jump)) { jpeg_destroy_decompress(&jpeg);return 0; }
    jpeg_create_decompress(&jpeg);jpeg_mem_src(&jpeg,input,length);
    jpeg_read_header(&jpeg,TRUE);
    if(jpeg.image_width!=width || jpeg.image_height!=height) { jpeg_destroy_decompress(&jpeg);return 0; }
    jpeg.scale_num=1;jpeg.scale_denom=denominator;jpeg.out_color_space=JCS_EXT_RGBA;
    monitor.base.progress_monitor=progress;monitor.row=row;monitor.context=context;jpeg.progress=&monitor.base;
    jpeg_start_decompress(&jpeg);
    if(left>=right || top>=bottom || right>jpeg.output_width || bottom>jpeg.output_height) { jpeg_destroy_decompress(&jpeg);return 0; }
    JDIMENSION x=left,w=right-left;jpeg_crop_scanline(&jpeg,&x,&w);
    JSAMPARRAY pixels=(*jpeg.mem->alloc_sarray)((j_common_ptr)&jpeg,JPOOL_IMAGE,w*4,1);
    while(jpeg.output_scanline<top) {
        unsigned n=top-jpeg.output_scanline;if(n>128)n=128;
        if(jpeg_skip_scanlines(&jpeg,n)!=n || !row(context,0,0,NULL,0)) { jpeg_destroy_decompress(&jpeg);return 0; }
    }
    while(jpeg.output_scanline<bottom) {
        unsigned y=jpeg.output_scanline;
        if(jpeg_read_scanlines(&jpeg,pixels,1)!=1 || !row(context,x,y,pixels[0],w)) { jpeg_destroy_decompress(&jpeg);return 0; }
    }
    jpeg_destroy_decompress(&jpeg);return 1;
}
