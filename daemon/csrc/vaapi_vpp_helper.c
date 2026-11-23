#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <va/va.h>
#include <va/va_vpp.h>

typedef struct ScrexVaVpp {
    VADisplay display;
    VAConfigID config;
    VAContextID context;
} ScrexVaVpp;

ScrexVaVpp *screx_va_vpp_create(void *va_display, int width, int height) {
    if (!va_display || width <= 0 || height <= 0) {
        return NULL;
    }

    ScrexVaVpp *vpp = (ScrexVaVpp *)calloc(1, sizeof(ScrexVaVpp));
    if (!vpp) {
        return NULL;
    }
    vpp->display = (VADisplay)va_display;
    vpp->config = VA_INVALID_ID;
    vpp->context = VA_INVALID_ID;

    VAStatus st = vaCreateConfig(
        vpp->display,
        VAProfileNone,
        VAEntrypointVideoProc,
        NULL,
        0,
        &vpp->config
    );
    if (st != VA_STATUS_SUCCESS) {
        free(vpp);
        return NULL;
    }

    st = vaCreateContext(
        vpp->display,
        vpp->config,
        width,
        height,
        0,
        NULL,
        0,
        &vpp->context
    );
    if (st != VA_STATUS_SUCCESS) {
        vaDestroyConfig(vpp->display, vpp->config);
        free(vpp);
        return NULL;
    }

    return vpp;
}

void screx_va_vpp_destroy(ScrexVaVpp *vpp) {
    if (!vpp) {
        return;
    }
    if (vpp->context != VA_INVALID_ID) {
        vaDestroyContext(vpp->display, vpp->context);
    }
    if (vpp->config != VA_INVALID_ID) {
        vaDestroyConfig(vpp->display, vpp->config);
    }
    free(vpp);
}

/* Single-pass VA-API VPP: converts primary_surface (BGR0/BGRA) to dst_surface
   (NV12) and optionally alpha-blends cursor_surface (premultiplied ARGB8888)
   at (cursor_x, cursor_y) on the GPU with zero CPU pixel copies. */
int screx_va_vpp_compose(
    ScrexVaVpp *vpp,
    uint32_t dst_surface,
    uint32_t primary_surface,
    int frame_w,
    int frame_h,
    int has_cursor,
    uint32_t cursor_surface,
    int cursor_x,
    int cursor_y,
    int cursor_src_w,
    int cursor_src_h,
    int cursor_dst_w,
    int cursor_dst_h
) {
    if (!vpp || frame_w <= 0 || frame_h <= 0) {
        return -1;
    }

    VAStatus st = vaBeginPicture(vpp->display, vpp->context, (VASurfaceID)dst_surface);
    if (st != VA_STATUS_SUCCESS) {
        return -(int)st;
    }

    VARectangle full_rect;
    full_rect.x = 0;
    full_rect.y = 0;
    full_rect.width = (uint16_t)frame_w;
    full_rect.height = (uint16_t)frame_h;

    VAProcPipelineParameterBuffer primary_param;
    memset(&primary_param, 0, sizeof(primary_param));
    primary_param.surface = (VASurfaceID)primary_surface;
    primary_param.surface_region = &full_rect;
    primary_param.output_region = &full_rect;
    primary_param.output_background_color = 0xff000000;
    primary_param.surface_color_standard = VAProcColorStandardSRGB;
    primary_param.output_color_standard = VAProcColorStandardBT709;
    primary_param.output_color_properties.color_range = VA_SOURCE_RANGE_REDUCED;

    VABufferID primary_buf = VA_INVALID_ID;
    st = vaCreateBuffer(
        vpp->display,
        vpp->context,
        VAProcPipelineParameterBufferType,
        sizeof(primary_param),
        1,
        &primary_param,
        &primary_buf
    );
    if (st != VA_STATUS_SUCCESS) {
        vaEndPicture(vpp->display, vpp->context);
        return -(int)st;
    }

    st = vaRenderPicture(vpp->display, vpp->context, &primary_buf, 1);
    if (st != VA_STATUS_SUCCESS) {
        vaDestroyBuffer(vpp->display, primary_buf);
        vaEndPicture(vpp->display, vpp->context);
        return -(int)st;
    }

    VABufferID cursor_buf = VA_INVALID_ID;
    if (has_cursor && cursor_src_w > 0 && cursor_src_h > 0 &&
        cursor_dst_w > 0 && cursor_dst_h > 0) {
        int dst_x0 = cursor_x < 0 ? 0 : cursor_x;
        int dst_y0 = cursor_y < 0 ? 0 : cursor_y;
        int dst_x1 = cursor_x + cursor_dst_w;
        int dst_y1 = cursor_y + cursor_dst_h;
        if (dst_x1 > frame_w) dst_x1 = frame_w;
        if (dst_y1 > frame_h) dst_y1 = frame_h;

        if (dst_x1 > dst_x0 && dst_y1 > dst_y0) {
            int clip_left = dst_x0 - cursor_x;
            int clip_top = dst_y0 - cursor_y;
            int vis_dst_w = dst_x1 - dst_x0;
            int vis_dst_h = dst_y1 - dst_y0;

            int src_x = (int)((int64_t)clip_left * cursor_src_w / cursor_dst_w);
            int src_y = (int)((int64_t)clip_top * cursor_src_h / cursor_dst_h);
            int src_w = (int)((int64_t)vis_dst_w * cursor_src_w / cursor_dst_w);
            int src_h = (int)((int64_t)vis_dst_h * cursor_src_h / cursor_dst_h);
            if (src_w < 1) src_w = 1;
            if (src_h < 1) src_h = 1;
            if (src_x + src_w > cursor_src_w) src_w = cursor_src_w - src_x;
            if (src_y + src_h > cursor_src_h) src_h = cursor_src_h - src_y;

            if (src_w > 0 && src_h > 0) {
                VARectangle src_rect;
                src_rect.x = (int16_t)src_x;
                src_rect.y = (int16_t)src_y;
                src_rect.width = (uint16_t)src_w;
                src_rect.height = (uint16_t)src_h;

                VARectangle dst_rect;
                dst_rect.x = (int16_t)dst_x0;
                dst_rect.y = (int16_t)dst_y0;
                dst_rect.width = (uint16_t)vis_dst_w;
                dst_rect.height = (uint16_t)vis_dst_h;

                VABlendState blend;
                memset(&blend, 0, sizeof(blend));
                blend.flags = VA_BLEND_PREMULTIPLIED_ALPHA;

                VAProcPipelineParameterBuffer cursor_param;
                memset(&cursor_param, 0, sizeof(cursor_param));
                cursor_param.surface = (VASurfaceID)cursor_surface;
                cursor_param.surface_region = &src_rect;
                cursor_param.output_region = &dst_rect;
                cursor_param.blend_state = &blend;
                cursor_param.surface_color_standard = VAProcColorStandardSRGB;
                cursor_param.output_color_standard = VAProcColorStandardBT709;
                cursor_param.output_color_properties.color_range = VA_SOURCE_RANGE_REDUCED;

                st = vaCreateBuffer(
                    vpp->display,
                    vpp->context,
                    VAProcPipelineParameterBufferType,
                    sizeof(cursor_param),
                    1,
                    &cursor_param,
                    &cursor_buf
                );
                if (st == VA_STATUS_SUCCESS) {
                    st = vaRenderPicture(vpp->display, vpp->context, &cursor_buf, 1);
                }
            }
        }
    }

    VAStatus end_st = vaEndPicture(vpp->display, vpp->context);
    vaDestroyBuffer(vpp->display, primary_buf);
    if (cursor_buf != VA_INVALID_ID) {
        vaDestroyBuffer(vpp->display, cursor_buf);
    }

    if (st != VA_STATUS_SUCCESS) {
        return -(int)st;
    }
    if (end_st != VA_STATUS_SUCCESS) {
        return -(int)end_st;
    }
    return 0;
}
