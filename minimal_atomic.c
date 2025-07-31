// minimal_atomic.c  ── fixed for wider libdrm compatibility
#define _GNU_SOURCE
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <errno.h>
#include <ctype.h>

#include <drm.h>
#include <drm_fourcc.h>          /* <-- DRM_FORMAT_* */
#include <xf86drm.h>
#include <xf86drmMode.h>

/* -------------------------------------------------------- *
 * Helpers
 * -------------------------------------------------------- */
static uint32_t find_prop(int fd, uint32_t obj_id, uint32_t obj_type,
                          const char *name)
{
    drmModeObjectPropertiesPtr props = drmModeObjectGetProperties(fd, obj_id,
                                                                  obj_type);
    if (!props) return 0;

    uint32_t ret = 0;
    for (uint32_t i = 0; i < props->count_props; ++i) {
        drmModePropertyRes *p = drmModeGetProperty(fd, props->props[i]);
        if (p && !strcmp(p->name, name)) {
            ret = p->prop_id;
            drmModeFreeProperty(p);
            break;
        }
        if (p) drmModeFreeProperty(p);
    }
    drmModeFreeObjectProperties(props);
    return ret;
}

/* libdrm < 2.4.117 has no drmModeGetCrtcIndex ― do it ourselves */
static int get_crtc_index(const drmModeRes *res, uint32_t crtc_id)
{
    for (int i = 0; i < res->count_crtcs; ++i)
        if (res->crtcs[i] == crtc_id)
            return i;
    return -1;
}

/* -------------------------------------------------------- *
 * Main
 * -------------------------------------------------------- */
int main(int argc, char **argv) {
    const char *dev_path = "/dev/dri/card0";
    if (argc > 1) {
        if (argv[1][0] == '/') {                  /* full path given */
            dev_path = argv[1];
        } else {                                  /* numeric index */
            bool digits = true;
            for (const char *p = argv[1]; *p; ++p)
                if (!isdigit((unsigned char)*p)) { digits = false; break; }
            if (digits) {
                static char buf[32];
                snprintf(buf, sizeof(buf), "/dev/dri/card%s", argv[1]);
                dev_path = buf;
            } 
        }
    }

    int fd = open(dev_path, O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror(dev_path); return 1; }

    if (drmSetClientCap(fd, DRM_CLIENT_CAP_ATOMIC, 1)) {
        fprintf(stderr, "Atomic KMS not supported\n");
        return 1;
    }

    drmModeRes *res = drmModeGetResources(fd);
    if (!res) { perror("drmModeGetResources"); return 1; }

    /* ---------- connector + preferred mode ---------- */
    drmModeConnector *conn = NULL;
    drmModeModeInfo mode   = {0};
    uint32_t conn_id       = 0;

    for (int i = 0; i < res->count_connectors; ++i) {
        conn = drmModeGetConnector(fd, res->connectors[i]);
        if (conn && conn->connection == DRM_MODE_CONNECTED && conn->count_modes) {
            conn_id = conn->connector_id;
            mode    = conn->modes[0];
            break;
        }
        if (conn) { drmModeFreeConnector(conn); conn = NULL; }
    }
    if (!conn) { fprintf(stderr, "No connected connector\n"); return 1; }

    /* ---------- CRTC compatible with connector ---------- */
    drmModeEncoder *enc = drmModeGetEncoder(fd, conn->encoder_id);
    uint32_t crtc_id = enc ? enc->crtc_id : 0;

    if (!crtc_id && enc) {                 /* pick first possible CRTC */
        for (int i = 0; i < res->count_crtcs; ++i)
            if (enc->possible_crtcs & (1 << i)) {
                crtc_id = res->crtcs[i];
                break;
            }
    }
    if (enc) drmModeFreeEncoder(enc);
    if (!crtc_id) { fprintf(stderr, "No CRTC found\n"); return 1; }

    /* ---------- primary plane on that CRTC ---------- */
    drmModePlaneRes *pres = drmModeGetPlaneResources(fd);
    uint32_t plane_id = 0;
    int crtc_idx = get_crtc_index(res, crtc_id);

    for (uint32_t i = 0; i < pres->count_planes; ++i) {
        drmModePlane *pl = drmModeGetPlane(fd, pres->planes[i]);
        if (pl && (pl->possible_crtcs & (1 << crtc_idx))) {
            drmModeObjectPropertiesPtr props =
                drmModeObjectGetProperties(fd, pl->plane_id,
                                           DRM_MODE_OBJECT_PLANE);
            for (uint32_t j = 0; j < props->count_props; ++j) {
                drmModePropertyRes *pr = drmModeGetProperty(fd, props->props[j]);
                if (pr && (pr->flags & DRM_MODE_PROP_IMMUTABLE) &&
                    !strcmp(pr->name, "type") &&
                    props->prop_values[j] == DRM_PLANE_TYPE_PRIMARY) {
                    plane_id = pl->plane_id;
                    drmModeFreeProperty(pr);
                    break;
                }
                if (pr) drmModeFreeProperty(pr);
            }
            drmModeFreeObjectProperties(props);
        }
        if (pl) drmModeFreePlane(pl);
        if (plane_id) break;
    }
    drmModeFreePlaneResources(pres);
    if (!plane_id) { fprintf(stderr, "No primary plane\n"); return 1; }

    /* ---------- dumb buffer ---------- */
    struct drm_mode_create_dumb creq = {
        .width  = mode.hdisplay,
        .height = mode.vdisplay,
        .bpp    = 32,
    };
    if (drmIoctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &creq) < 0) {
        perror("CREATE_DUMB"); return 1;
    }

    uint32_t fb_id;
    const uint32_t handles[4] = { creq.handle };
    const uint32_t pitches[4] = { creq.pitch  };
    const uint32_t offsets[4] = { 0 };

    if (drmModeAddFB2(fd, creq.width, creq.height, DRM_FORMAT_XRGB8888,
                      handles, pitches, offsets, &fb_id, 0)) {
        perror("AddFB2"); return 1;
    }

    struct drm_mode_map_dumb mreq = { .handle = creq.handle };
    if (drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mreq) < 0) {
        perror("MAP_DUMB"); return 1;
    }

    uint32_t *pix = mmap(0, creq.size, PROT_READ | PROT_WRITE, MAP_SHARED,
                         fd, mreq.offset);
    if (pix == MAP_FAILED) { perror("mmap"); return 1; }

    /* fill simple vertical RGB bars */
    for (uint32_t y = 0; y < creq.height; ++y) {
        for (uint32_t x = 0; x < creq.width; ++x) {
            uint8_t bar = x * 3 / creq.width;
            uint32_t c  = bar == 0 ? 0x00FF0000 :
                          bar == 1 ? 0x0000FF00 : 0x000000FF;
            pix[y * (creq.pitch / 4) + x] = c;
        }
    }

    /* ---------- atomic commit ---------- */
    drmModeAtomicReq *req = drmModeAtomicAlloc();
    if (!req) { fprintf(stderr, "atomic alloc failed\n"); return 1; }

    uint32_t conn_prop_crtc_id = find_prop(fd, conn_id,
                                           DRM_MODE_OBJECT_CONNECTOR,
                                           "CRTC_ID");
    drmModeAtomicAddProperty(req, conn_id, conn_prop_crtc_id, crtc_id);

    uint32_t crtc_prop_active =
        find_prop(fd, crtc_id, DRM_MODE_OBJECT_CRTC, "ACTIVE");
    uint32_t crtc_prop_mode =
        find_prop(fd, crtc_id, DRM_MODE_OBJECT_CRTC, "MODE_ID");

    uint32_t mode_blob;
    drmModeCreatePropertyBlob(fd, &mode, sizeof(mode), &mode_blob);
    drmModeAtomicAddProperty(req, crtc_id, crtc_prop_active, 1);
    drmModeAtomicAddProperty(req, crtc_id, crtc_prop_mode, mode_blob);

    uint32_t plane_prop_fb   = find_prop(fd, plane_id,
                                         DRM_MODE_OBJECT_PLANE, "FB_ID");
    uint32_t plane_prop_crtc = find_prop(fd, plane_id,
                                         DRM_MODE_OBJECT_PLANE, "CRTC_ID");
    uint32_t plane_prop_src_x = find_prop(fd, plane_id,
                                          DRM_MODE_OBJECT_PLANE, "SRC_X");
    uint32_t plane_prop_src_y = find_prop(fd, plane_id,
                                          DRM_MODE_OBJECT_PLANE, "SRC_Y");
    uint32_t plane_prop_src_w = find_prop(fd, plane_id,
                                          DRM_MODE_OBJECT_PLANE, "SRC_W");
    uint32_t plane_prop_src_h = find_prop(fd, plane_id,
                                          DRM_MODE_OBJECT_PLANE, "SRC_H");
    uint32_t plane_prop_crtc_x = find_prop(fd, plane_id,
                                           DRM_MODE_OBJECT_PLANE, "CRTC_X");
    uint32_t plane_prop_crtc_y = find_prop(fd, plane_id,
                                           DRM_MODE_OBJECT_PLANE, "CRTC_Y");
    uint32_t plane_prop_crtc_w = find_prop(fd, plane_id,
                                           DRM_MODE_OBJECT_PLANE, "CRTC_W");
    uint32_t plane_prop_crtc_h = find_prop(fd, plane_id,
                                           DRM_MODE_OBJECT_PLANE, "CRTC_H");

    drmModeAtomicAddProperty(req, plane_id, plane_prop_fb, fb_id);
    drmModeAtomicAddProperty(req, plane_id, plane_prop_crtc, crtc_id);
    drmModeAtomicAddProperty(req, plane_id, plane_prop_src_x, 0);
    drmModeAtomicAddProperty(req, plane_id, plane_prop_src_y, 0);
    drmModeAtomicAddProperty(req, plane_id, plane_prop_src_w,
                             mode.hdisplay << 16);
    drmModeAtomicAddProperty(req, plane_id, plane_prop_src_h,
                             mode.vdisplay << 16);
    drmModeAtomicAddProperty(req, plane_id, plane_prop_crtc_x, 0);
    drmModeAtomicAddProperty(req, plane_id, plane_prop_crtc_y, 0);
    drmModeAtomicAddProperty(req, plane_id, plane_prop_crtc_w,
                             mode.hdisplay);
    drmModeAtomicAddProperty(req, plane_id, plane_prop_crtc_h,
                             mode.vdisplay);

    if (drmModeAtomicCommit(fd, req,
            DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_ALLOW_MODESET, NULL) < 0) {
        perror("atomic commit"); return 1;
    }

    puts("RGB test pattern shown for 5 s…");
    sleep(5);

    /* ---------- tidy up ---------- */
    drmModeAtomicFree(req);
    drmModeDestroyPropertyBlob(fd, mode_blob);
    munmap(pix, creq.size);
    drmModeRmFB(fd, fb_id);

    struct drm_mode_destroy_dumb dreq = { .handle = creq.handle };
    drmIoctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &dreq);

    drmModeFreeConnector(conn);
    drmModeFreeResources(res);
    close(fd);
    return 0;
}
