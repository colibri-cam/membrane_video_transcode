#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <string.h>
#include <stdint.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

#define DISPLAY_TIME 20 // seconds to display the image

// Helpers to get property IDs (obj_type is one of DRM_MODE_OBJECT_* constants)
uint32_t find_prop_id(int fd, uint32_t obj_id, uint32_t obj_type, const char *name) {
    drmModeObjectProperties *props = drmModeObjectGetProperties(fd, obj_id, obj_type);
    if (!props) return 0;
    uint32_t id = 0;
    for (uint32_t i = 0; i < props->count_props; i++) {
        drmModePropertyRes *prop = drmModeGetProperty(fd, props->props[i]);
        if (prop) {
            if (strcmp(prop->name, name) == 0) {
                id = prop->prop_id;
                drmModeFreeProperty(prop);
                break;
            }
            drmModeFreeProperty(prop);
        }
    }
    drmModeFreeObjectProperties(props);
    return id;
}

int main(int argc, char *argv[]) {
    const char *card = "/dev/dri/card0";
    if (argc > 1) {
        card = argv[1];
    }

    int fd = open(card, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        perror("open");
        fprintf(stderr, "Failed to open DRM device '%s'\n", card);
        return 1;
    }

    drmModeRes *res = drmModeGetResources(fd);
    if (!res) { perror("drmModeGetResources"); close(fd); return 1; }

    // Pick first connected connector
    drmModeConnector *conn = NULL;
    for (int i = 0; i < res->count_connectors; i++) {
        conn = drmModeGetConnector(fd, res->connectors[i]);
        if (conn && conn->connection == DRM_MODE_CONNECTED) break;
        if (conn) drmModeFreeConnector(conn);
        conn = NULL;
    }
    if (!conn) {
        fprintf(stderr, "No connected connector found on %s\n", card);
        drmModeFreeResources(res);
        close(fd);
        return 1;
    }

    // Try to get encoder; may be null if driver hasn't bound connector
    drmModeEncoder *enc = NULL;
    for (int i = 0; conn && i < conn->count_encoders; i++) {
        enc = drmModeGetEncoder(fd, conn->encoders[i]);
        if (enc) break;
    }

    // Determine CRTC ID: use encoder->crtc_id or fallback to first available CRTC
    uint32_t chosen_crtc_id = 0;
    if (enc && enc->crtc_id) {
        chosen_crtc_id = enc->crtc_id;
    } else if (res->count_crtcs > 0) {
        chosen_crtc_id = res->crtcs[0];
    }

    if (!chosen_crtc_id) {
        fprintf(stderr, "No CRTC available for %s\n", card);
        if (enc) drmModeFreeEncoder(enc);
        drmModeFreeConnector(conn);
        drmModeFreeResources(res);
        close(fd);
        return 1;
    }

    drmModeCrtc *crtc = drmModeGetCrtc(fd, chosen_crtc_id);
    if (!crtc) {
        fprintf(stderr, "Failed to get CRTC %u on %s\n", chosen_crtc_id, card);
        if (enc) drmModeFreeEncoder(enc);
        drmModeFreeConnector(conn);
        drmModeFreeResources(res);
        close(fd);
        return 1;
    }

    drmModeModeInfo mode = conn->modes[0];
    uint32_t width = mode.hdisplay, height = mode.vdisplay;

    // Create dumb buffer
    struct drm_mode_create_dumb creq = {0};
    creq.width = width;
    creq.height = height;
    creq.bpp = 32;
    if (drmIoctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &creq) < 0) {
        perror("DRM_IOCTL_MODE_CREATE_DUMB");
        goto cleanup_crtc;
    }

    // Map buffer
    struct drm_mode_map_dumb mreq = { .handle = creq.handle };
    if (drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mreq) < 0) {
        perror("DRM_IOCTL_MODE_MAP_DUMB");
        goto cleanup_dumb;
    }
    void *map = mmap(NULL, creq.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, mreq.offset);
    if (map == MAP_FAILED) { perror("mmap"); goto cleanup_dumb; }

    // Fill with a simple gradient
    uint32_t *pix = map;
    for (uint32_t y = 0; y < height; y++) {
        for (uint32_t x = 0; x < width; x++) {
            uint32_t r = (x * 255) / width;
            uint32_t g = (y * 255) / height;
            uint32_t b = 255 - ((r + g) / 2);
            pix[y * (creq.pitch/4) + x] = (r << 16) | (g << 8) | b;
        }
    }

    // Create framebuffer
    uint32_t fb;
    if (drmModeAddFB(fd, width, height, 24, 32, creq.pitch, creq.handle, &fb) < 0) {
        perror("drmModeAddFB");
        munmap(map, creq.size);
        goto cleanup_dumb;
    }

    // Determine CRTC index for plane mask
    int crtc_index = -1;
    for (int i = 0; i < res->count_crtcs; i++) {
        if (res->crtcs[i] == chosen_crtc_id) {
            crtc_index = i;
            break;
        }
    }
    if (crtc_index < 0) {
        fprintf(stderr, "CRTC index not found\n");
        goto cleanup_fb;
    }

    // Find primary plane compatible with this CRTC
    drmModePlaneRes *pres = drmModeGetPlaneResources(fd);
    uint32_t plane_id = 0;
    for (uint32_t i = 0; pres && i < pres->count_planes; i++) {
        drmModePlane *p = drmModeGetPlane(fd, pres->planes[i]);
        if (p && (p->possible_crtcs & (1 << crtc_index))) {
            plane_id = p->plane_id;
            drmModeFreePlane(p);
            break;
        }
        if (p) drmModeFreePlane(p);
    }
    drmModeFreePlaneResources(pres);
    if (!plane_id) {
        fprintf(stderr, "No plane found\n");
        goto cleanup_fb;
    }

    // Create mode blob
    uint32_t mode_blob;
    if (drmModeCreatePropertyBlob(fd, &mode, sizeof(mode), &mode_blob) < 0) {
        perror("drmModeCreatePropertyBlob");
        goto cleanup_fb;
    }

    // Setup atomic request
    drmModeAtomicReq *req = drmModeAtomicAlloc();
    if (!req) { fprintf(stderr, "Atomic alloc failed\n"); goto cleanup_blob; }

    // Connector: set CRTC_ID
    drmModeAtomicAddProperty(req, conn->connector_id,
        find_prop_id(fd, conn->connector_id, DRM_MODE_OBJECT_CONNECTOR, "CRTC_ID"),
        chosen_crtc_id);

    // CRTC: set MODE_ID, ACTIVE
    drmModeAtomicAddProperty(req, chosen_crtc_id,
        find_prop_id(fd, chosen_crtc_id, DRM_MODE_OBJECT_CRTC, "MODE_ID"),
        mode_blob);
    drmModeAtomicAddProperty(req, chosen_crtc_id,
        find_prop_id(fd, chosen_crtc_id, DRM_MODE_OBJECT_CRTC, "ACTIVE"),
        1);

    // Plane props: source and destination
    drmModeAtomicAddProperty(req, plane_id,
        find_prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "CRTC_ID"),
        chosen_crtc_id);
    drmModeAtomicAddProperty(req, plane_id,
        find_prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "FB_ID"),
        fb);
    drmModeAtomicAddProperty(req, plane_id,
        find_prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "SRC_X"), 0);
    drmModeAtomicAddProperty(req, plane_id,
        find_prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "SRC_Y"), 0);
    drmModeAtomicAddProperty(req, plane_id,
        find_prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "SRC_W"), width << 16);
    drmModeAtomicAddProperty(req, plane_id,
        find_prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "SRC_H"), height << 16);
    drmModeAtomicAddProperty(req, plane_id,
        find_prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "CRTC_X"), 0);
    drmModeAtomicAddProperty(req, plane_id,
        find_prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "CRTC_Y"), 0);
    drmModeAtomicAddProperty(req, plane_id,
        find_prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "CRTC_W"), width);
    drmModeAtomicAddProperty(req, plane_id,
        find_prop_id(fd, plane_id, DRM_MODE_OBJECT_PLANE, "CRTC_H"), height);

    // Commit atomically
    if (drmModeAtomicCommit(fd, req, DRM_MODE_ATOMIC_ALLOW_MODESET, NULL) < 0) {
        perror("drmModeAtomicCommit");
    }

    sleep(DISPLAY_TIME);

    // Cleanup
    drmModeAtomicFree(req);
cleanup_blob:
    drmModeDestroyPropertyBlob(fd, mode_blob);
cleanup_fb:
    drmModeRmFB(fd, fb);
    munmap(map, creq.size);
cleanup_dumb:
    {
        struct drm_mode_destroy_dumb dreq = { .handle = creq.handle };
        drmIoctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &dreq);
    }
cleanup_crtc:
    drmModeFreeCrtc(crtc);
    if (enc) drmModeFreeEncoder(enc);
    drmModeFreeConnector(conn);
    drmModeFreeResources(res);
    close(fd);
    return 0;
}

/*
 * Compile on Arch/Linux with:
 *   gcc minimal_drm_atomic_example.c -o minimal_drm_atomic_example \
 *       -I/usr/include/libdrm -ldrm
 */
