#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <string.h>
#include <stdint.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

#define DISPLAY_TIME 10 // seconds to display the image

int main(int argc, char *argv[]) {
    const char *card = "/dev/dri/card0";
    if (argc > 1) card = argv[1];

    int fd = open(card, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        perror("open");
        return 1;
    }

    drmModeRes *res = drmModeGetResources(fd);
    if (!res) { perror("drmModeGetResources"); close(fd); return 1; }

    // Find first connected connector
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

    // Use first encoder of the connector
    drmModeEncoder *enc = NULL;
    if (conn->count_encoders > 0) {
        enc = drmModeGetEncoder(fd, conn->encoders[0]);
    }
    if (!enc || !enc->crtc_id) {
        fprintf(stderr, "No valid encoder for connector %u\n", conn->connector_id);
        drmModeFreeConnector(conn);
        drmModeFreeResources(res);
        close(fd);
        return 1;
    }

    // Get CRTC
    drmModeCrtc *crtc = drmModeGetCrtc(fd, enc->crtc_id);
    if (!crtc) {
        fprintf(stderr, "Failed to get CRTC %u\n", enc->crtc_id);
        drmModeFreeEncoder(enc);
        drmModeFreeConnector(conn);
        drmModeFreeResources(res);
        close(fd);
        return 1;
    }

    // Use the first mode
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

    // Map the buffer into userspace
    struct drm_mode_map_dumb mreq = { .handle = creq.handle };
    if (drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mreq) < 0) {
        perror("DRM_IOCTL_MODE_MAP_DUMB");
        goto destroy_dumb;
    }
    void *map = mmap(NULL, creq.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, mreq.offset);
    if (map == MAP_FAILED) {
        perror("mmap");
        goto destroy_dumb;
    }

    // Fill with gradient
    uint32_t *pixels = map;
    for (uint32_t y = 0; y < height; y++) {
        for (uint32_t x = 0; x < width; x++) {
            uint32_t r = (x * 255) / width;
            uint32_t g = (y * 255) / height;
            uint32_t b = 255 - ((r + g) / 2);
            pixels[y * (creq.pitch/4) + x] = (r << 16) | (g << 8) | b;
        }
    }

    // Create framebuffer
    uint32_t fb;
    if (drmModeAddFB(fd, width, height, 24, 32, creq.pitch, creq.handle, &fb) < 0) {
        perror("drmModeAddFB");
        munmap(map, creq.size);
        goto destroy_dumb;
    }

    // Modeset with legacy API
    if (drmModeSetCrtc(fd, crtc->crtc_id, fb, 0, 0,
                       &conn->connector_id, 1, &mode) < 0) {
        perror("drmModeSetCrtc");
    }

    // Display for a while
    sleep(DISPLAY_TIME);

    // Cleanup
    drmModeRmFB(fd, fb);
    munmap(map, creq.size);

destroy_dumb:
    {
        struct drm_mode_destroy_dumb dreq = { .handle = creq.handle };
        drmIoctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &dreq);
    }

cleanup_crtc:
    drmModeFreeCrtc(crtc);
    drmModeFreeEncoder(enc);
    drmModeFreeConnector(conn);
    drmModeFreeResources(res);
    close(fd);
    return 0;
}

/*
 * Compile on Arch/Linux with:
 *   gcc minimal_drm_legacy_example.c -o minimal_drm_example \
 *       -I/usr/include/libdrm -ldrm
 */
