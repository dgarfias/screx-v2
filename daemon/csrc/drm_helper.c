#include <drm/drm.h>
#include <drm/drm_fourcc.h>
#include <drm/drm_mode.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

/* Matches struct drm_evdi_grabpix in evdi_drm.h (driver ioctl 0x02). */
#define SCREX_DRM_EVDI_GRABPIX 0x02

struct screx_drm_evdi_grabpix {
    int32_t mode;
    int32_t buf_width;
    int32_t buf_height;
    int32_t buf_byte_stride;
    unsigned char *buffer;
    int32_t num_rects;
    struct drm_clip_rect *rects;
};

#define SCREX_DRM_IOCTL_EVDI_GRABPIX \
    DRM_IOWR(DRM_COMMAND_BASE + SCREX_DRM_EVDI_GRABPIX, struct screx_drm_evdi_grabpix)

typedef struct {
    uint32_t plane_id;
    uint32_t prop_crtc_x;
    uint32_t prop_crtc_y;
    uint32_t prop_crtc_w;
    uint32_t prop_crtc_h;
} ScrexDrmCursorPlane;

typedef struct {
    uint32_t fb_id;
    uint32_t width;
    uint32_t height;
    uint32_t pixel_format;
    uint32_t flags;
    uint32_t num_objects;
    uint32_t num_planes;
    int32_t  fds[4];
    uint64_t object_sizes[4];
    uint64_t object_modifiers[4];
    uint32_t plane_obj_idx[4];
    uint32_t pitches[4];
    uint32_t offsets[4];

    /* Optional hardware cursor plane export (cursor_fd >= 0 when present). */
    uint32_t cursor_fb_id;
    int32_t  cursor_x;
    int32_t  cursor_y;
    uint32_t cursor_width;
    uint32_t cursor_height;
    uint32_t cursor_crtc_w;
    uint32_t cursor_crtc_h;
    uint32_t cursor_pixel_format;
    int32_t  cursor_fd;
    uint64_t cursor_size;
    uint64_t cursor_modifier;
    uint32_t cursor_pitch;
    uint32_t cursor_offset;
} ScrexDrmExport;

static int drm_ioctl_retry(int fd, unsigned long request, void *arg) {
    int ret;
    do {
        ret = ioctl(fd, request, arg);
    } while (ret == -1 && (errno == EINTR || errno == EAGAIN));
    return ret;
}

static void gem_close_handle(int drm_fd, uint32_t handle) {
    if (handle == 0) {
        return;
    }
    struct drm_gem_close close_req;
    memset(&close_req, 0, sizeof(close_req));
    close_req.handle = handle;
    (void)drm_ioctl_retry(drm_fd, DRM_IOCTL_GEM_CLOSE, &close_req);
}

static int prime_export_handle(int drm_fd, uint32_t handle, int32_t *out_fd) {
    struct drm_prime_handle prime;
    memset(&prime, 0, sizeof(prime));
    prime.handle = handle;
    prime.flags = DRM_CLOEXEC | DRM_RDWR;
    prime.fd = -1;
    if (drm_ioctl_retry(drm_fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &prime) < 0) {
        memset(&prime, 0, sizeof(prime));
        prime.handle = handle;
        prime.flags = DRM_CLOEXEC;
        prime.fd = -1;
        if (drm_ioctl_retry(drm_fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &prime) < 0) {
            return -errno;
        }
    }
    *out_fd = prime.fd;
    return 0;
}

static void find_cursor_plane(int drm_fd, ScrexDrmCursorPlane *out_cursor) {
    memset(out_cursor, 0, sizeof(*out_cursor));
    struct drm_mode_get_plane_res pres;
    memset(&pres, 0, sizeof(pres));
    if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pres) < 0 ||
        pres.count_planes == 0) {
        return;
    }

    uint32_t planes[16];
    uint32_t count = pres.count_planes < 16 ? pres.count_planes : 16;
    memset(&pres, 0, sizeof(pres));
    pres.plane_id_ptr = (uint64_t)(uintptr_t)planes;
    pres.count_planes = count;
    if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pres) < 0) {
        return;
    }

    uint32_t actual = pres.count_planes < count ? pres.count_planes : count;
    for (uint32_t i = 0; i < actual; i++) {
        struct drm_mode_obj_get_properties oprops;
        uint32_t prop_ids[32];
        uint64_t prop_vals[32];
        memset(&oprops, 0, sizeof(oprops));
        oprops.obj_id = planes[i];
        oprops.obj_type = DRM_MODE_OBJECT_PLANE;
        oprops.props_ptr = (uint64_t)(uintptr_t)prop_ids;
        oprops.prop_values_ptr = (uint64_t)(uintptr_t)prop_vals;
        oprops.count_props = 32;
        if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &oprops) < 0) {
            continue;
        }
        uint32_t nprops = oprops.count_props < 32 ? oprops.count_props : 32;
        int is_cursor = 0;
        uint32_t prop_x = 0, prop_y = 0, prop_w = 0, prop_h = 0;
        for (uint32_t p = 0; p < nprops; p++) {
            struct drm_mode_get_property prop;
            memset(&prop, 0, sizeof(prop));
            prop.prop_id = prop_ids[p];
            if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_GETPROPERTY, &prop) < 0) {
                continue;
            }
            if (strcmp(prop.name, "type") == 0 && prop_vals[p] == 2) {
                is_cursor = 1;
            } else if (strcmp(prop.name, "CRTC_X") == 0) {
                prop_x = prop_ids[p];
            } else if (strcmp(prop.name, "CRTC_Y") == 0) {
                prop_y = prop_ids[p];
            } else if (strcmp(prop.name, "CRTC_W") == 0) {
                prop_w = prop_ids[p];
            } else if (strcmp(prop.name, "CRTC_H") == 0) {
                prop_h = prop_ids[p];
            }
        }
        if (is_cursor) {
            out_cursor->plane_id = planes[i];
            out_cursor->prop_crtc_x = prop_x;
            out_cursor->prop_crtc_y = prop_y;
            out_cursor->prop_crtc_w = prop_w;
            out_cursor->prop_crtc_h = prop_h;
            return;
        }
    }
}

static uint32_t find_first_crtc_id(int drm_fd) {
    struct drm_mode_card_res res;
    memset(&res, 0, sizeof(res));
    if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_GETRESOURCES, &res) < 0 ||
        res.count_crtcs == 0) {
        return 0;
    }

    uint32_t crtcs[16];
    uint32_t count = res.count_crtcs < 16 ? res.count_crtcs : 16;
    memset(&res, 0, sizeof(res));
    res.crtc_id_ptr = (uint64_t)(uintptr_t)crtcs;
    res.count_crtcs = count;
    if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_GETRESOURCES, &res) < 0) {
        return 0;
    }
    if (res.count_crtcs == 0) {
        return 0;
    }
    return crtcs[0];
}

int screx_drm_open_card(
    int card_index,
    uint32_t *out_crtc_id,
    ScrexDrmCursorPlane *out_cursor
) {
    char path[64];
    snprintf(path, sizeof(path), "/dev/dri/card%d", card_index);
    int fd = open(path, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        return -errno;
    }

    struct drm_set_client_cap cap;
    memset(&cap, 0, sizeof(cap));
    cap.capability = DRM_CLIENT_CAP_UNIVERSAL_PLANES;
    cap.value = 1;
    (void)drm_ioctl_retry(fd, DRM_IOCTL_SET_CLIENT_CAP, &cap);

    /* Atomic cap is required so DRM_IOCTL_MODE_OBJ_GETPROPERTIES exposes
       atomic plane properties (CRTC_X, CRTC_Y, CRTC_W, CRTC_H). */
    memset(&cap, 0, sizeof(cap));
    cap.capability = DRM_CLIENT_CAP_ATOMIC;
    cap.value = 1;
    (void)drm_ioctl_retry(fd, DRM_IOCTL_SET_CLIENT_CAP, &cap);

    if (out_crtc_id) {
        *out_crtc_id = find_first_crtc_id(fd);
    }
    if (out_cursor) {
        find_cursor_plane(fd, out_cursor);
    }
    return fd;
}

static void export_cursor_plane(
    int drm_fd,
    const ScrexDrmCursorPlane *cursor_plane,
    ScrexDrmExport *out
) {
    if (!cursor_plane || cursor_plane->plane_id == 0) {
        return;
    }

    struct drm_mode_get_plane plane;
    memset(&plane, 0, sizeof(plane));
    plane.plane_id = cursor_plane->plane_id;
    if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_GETPLANE, &plane) < 0 || plane.fb_id == 0) {
        return;
    }

    int32_t cx = 0, cy = 0;
    uint32_t cw = 0, ch = 0;
    struct drm_mode_obj_get_properties oprops;
    uint32_t prop_ids[32];
    uint64_t prop_vals[32];
    memset(&oprops, 0, sizeof(oprops));
    oprops.obj_id = cursor_plane->plane_id;
    oprops.obj_type = DRM_MODE_OBJECT_PLANE;
    oprops.props_ptr = (uint64_t)(uintptr_t)prop_ids;
    oprops.prop_values_ptr = (uint64_t)(uintptr_t)prop_vals;
    oprops.count_props = 32;
    if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &oprops) == 0) {
        uint32_t nprops = oprops.count_props < 32 ? oprops.count_props : 32;
        for (uint32_t p = 0; p < nprops; p++) {
            if (prop_ids[p] == cursor_plane->prop_crtc_x) {
                cx = (int32_t)prop_vals[p];
            } else if (prop_ids[p] == cursor_plane->prop_crtc_y) {
                cy = (int32_t)prop_vals[p];
            } else if (prop_ids[p] == cursor_plane->prop_crtc_w) {
                cw = (uint32_t)prop_vals[p];
            } else if (prop_ids[p] == cursor_plane->prop_crtc_h) {
                ch = (uint32_t)prop_vals[p];
            }
        }
    }

    struct drm_mode_fb_cmd2 fb2;
    memset(&fb2, 0, sizeof(fb2));
    fb2.fb_id = plane.fb_id;
    if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_GETFB2, &fb2) < 0) {
        return;
    }

    uint32_t unique_handles[4] = {0, 0, 0, 0};
    uint32_t num_unique = 0;
    for (uint32_t p = 0; p < 4; p++) {
        uint32_t h = fb2.handles[p];
        if (h == 0) {
            break;
        }
        int seen = 0;
        for (uint32_t u = 0; u < num_unique; u++) {
            if (unique_handles[u] == h) {
                seen = 1;
                break;
            }
        }
        if (!seen) {
            unique_handles[num_unique++] = h;
        }
    }

    int32_t cursor_fd = -1;
    if (num_unique > 0 && prime_export_handle(drm_fd, unique_handles[0], &cursor_fd) == 0) {
        int has_mod = (fb2.flags & DRM_MODE_FB_MODIFIERS) != 0;
        off_t sz = lseek(cursor_fd, 0, SEEK_END);
        uint64_t fallback_sz = (uint64_t)fb2.offsets[0] + (uint64_t)fb2.pitches[0] * (uint64_t)fb2.height;
        if (sz > 0) {
            (void)lseek(cursor_fd, 0, SEEK_SET);
        }
        out->cursor_fb_id = fb2.fb_id;
        out->cursor_x = cx;
        out->cursor_y = cy;
        out->cursor_width = fb2.width;
        out->cursor_height = fb2.height;
        out->cursor_crtc_w = cw > 0 ? cw : fb2.width;
        out->cursor_crtc_h = ch > 0 ? ch : fb2.height;
        out->cursor_pixel_format = fb2.pixel_format;
        out->cursor_fd = cursor_fd;
        out->cursor_size = sz > 0 ? (uint64_t)sz : fallback_sz;
        out->cursor_modifier = has_mod ? fb2.modifier[0] : DRM_FORMAT_MOD_INVALID;
        out->cursor_pitch = fb2.pitches[0];
        out->cursor_offset = fb2.offsets[0];
    }

    for (uint32_t u = 0; u < num_unique; u++) {
        gem_close_handle(drm_fd, unique_handles[u]);
    }
}

int screx_drm_export_fb(
    int drm_fd,
    uint32_t *io_crtc_id,
    const ScrexDrmCursorPlane *cursor_plane,
    ScrexDrmExport *out
) {
    if (!out || !io_crtc_id) {
        return -EINVAL;
    }
    memset(out, 0, sizeof(*out));
    for (int i = 0; i < 4; i++) {
        out->fds[i] = -1;
    }
    out->cursor_fd = -1;

    uint32_t crtc_id = *io_crtc_id;
    if (crtc_id == 0) {
        crtc_id = find_first_crtc_id(drm_fd);
        if (crtc_id == 0) {
            return -EAGAIN;
        }
        *io_crtc_id = crtc_id;
    }

    struct drm_mode_crtc crtc;
    memset(&crtc, 0, sizeof(crtc));
    crtc.crtc_id = crtc_id;
    if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_GETCRTC, &crtc) < 0) {
        return -errno;
    }
    if (crtc.fb_id == 0) {
        return -EAGAIN;
    }

    struct drm_mode_fb_cmd2 fb2;
    memset(&fb2, 0, sizeof(fb2));
    fb2.fb_id = crtc.fb_id;
    if (drm_ioctl_retry(drm_fd, DRM_IOCTL_MODE_GETFB2, &fb2) < 0) {
        return -errno;
    }

    uint32_t unique_handles[4] = {0, 0, 0, 0};
    uint64_t unique_modifiers[4] = {0, 0, 0, 0};
    uint64_t fallback_sizes[4] = {0, 0, 0, 0};
    uint32_t num_unique = 0;
    uint32_t num_planes = 0;
    int has_modifiers = (fb2.flags & DRM_MODE_FB_MODIFIERS) != 0;

    for (uint32_t p = 0; p < 4; p++) {
        uint32_t h = fb2.handles[p];
        if (h == 0) {
            break;
        }
        uint32_t obj_idx = num_unique;
        for (uint32_t u = 0; u < num_unique; u++) {
            if (unique_handles[u] == h) {
                obj_idx = u;
                break;
            }
        }
        if (obj_idx == num_unique) {
            unique_handles[num_unique] = h;
            unique_modifiers[num_unique] =
                has_modifiers ? fb2.modifier[p] : DRM_FORMAT_MOD_INVALID;
            num_unique++;
        }
        uint64_t plane_end = (uint64_t)fb2.offsets[p] + (uint64_t)fb2.pitches[p] * (uint64_t)fb2.height;
        if (plane_end > fallback_sizes[obj_idx]) {
            fallback_sizes[obj_idx] = plane_end;
        }
        out->plane_obj_idx[num_planes] = obj_idx;
        out->pitches[num_planes] = fb2.pitches[p];
        out->offsets[num_planes] = fb2.offsets[p];
        num_planes++;
    }

    if (num_unique == 0 || num_planes == 0) {
        return -ENOENT;
    }

    int err = 0;
    for (uint32_t u = 0; u < num_unique; u++) {
        int32_t fd = -1;
        err = prime_export_handle(drm_fd, unique_handles[u], &fd);
        if (err != 0) {
            break;
        }
        out->fds[u] = fd;
        out->object_modifiers[u] = unique_modifiers[u];
        off_t sz = lseek(fd, 0, SEEK_END);
        if (sz > 0) {
            out->object_sizes[u] = (uint64_t)sz;
            (void)lseek(fd, 0, SEEK_SET);
        } else {
            out->object_sizes[u] = fallback_sizes[u];
        }
    }

    /* Always close every GEM handle returned by DRM_IOCTL_MODE_GETFB2. */
    for (uint32_t u = 0; u < num_unique; u++) {
        gem_close_handle(drm_fd, unique_handles[u]);
    }

    if (err != 0) {
        for (uint32_t u = 0; u < 4; u++) {
            if (out->fds[u] >= 0) {
                close(out->fds[u]);
                out->fds[u] = -1;
            }
        }
        return err;
    }

    out->fb_id = fb2.fb_id;
    out->width = fb2.width;
    out->height = fb2.height;
    out->pixel_format = fb2.pixel_format;
    out->flags = fb2.flags;
    out->num_objects = num_unique;
    out->num_planes = num_planes;

    /* Best-effort hardware cursor plane export (zero-copy DMA-BUF). */
    export_cursor_plane(drm_fd, cursor_plane, out);
    return 0;
}

/* Acknowledge an EVDI frame update and dispatch the compositor's pending vblank
   without running dma_buf_begin_cpu_access() or copy_primary_pixels(). */
int screx_evdi_ack_vblank(int evdi_fd) {
    struct drm_clip_rect dummy_rect = {0, 0, 0, 0};
    struct screx_drm_evdi_grabpix cmd;
    memset(&cmd, 0, sizeof(cmd));
    cmd.mode = 1; /* EVDI_GRABPIX_MODE_DIRTY */
    cmd.buf_width = 0;
    cmd.buf_height = 0;
    cmd.buf_byte_stride = 0;
    cmd.buffer = NULL;
    cmd.num_rects = 1;
    cmd.rects = &dummy_rect;

    int ret;
    do {
        ret = ioctl(evdi_fd, SCREX_DRM_IOCTL_EVDI_GRABPIX, &cmd);
    } while (ret == -1 && errno == EINTR);

    if (ret == 0 || (ret == -1 && errno == EINVAL)) {
        return 0;
    }
    return -errno;
}
