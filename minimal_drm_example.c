#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

int main() {
    /* Open the DRM device */
    int fd = open("/dev/dri/card1", O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror("open"); return 1; }

    /* Get DRM resources */
    drmModeRes *resources = drmModeGetResources(fd);
    if (!resources) { perror("drmModeGetResources"); return 1; }

    /* Find a connected connector */
    drmModeConnector *conn = NULL;
    for (int i = 0; i < resources->count_connectors; i++) {
        conn = drmModeGetConnector(fd, resources->connectors[i]);
        if (conn->connection == DRM_MODE_CONNECTED) 
            break;
        drmModeFreeConnector(conn);
        conn = NULL;
    }
    if (!conn) { fprintf(stderr, "No connected connector found\n"); return 1; }

    /* Get encoder, CRTC and mode */
    drmModeEncoder *enc = drmModeGetEncoder(fd, conn->encoder_id);
    drmModeCrtc    *crtc = drmModeGetCrtc(fd, enc->crtc_id);
    drmModeModeInfo mode = conn->modes[0];
    uint32_t width  = mode.hdisplay;
    uint32_t height = mode.vdisplay;

    /* Create a dumb buffer */
    struct drm_mode_create_dumb creq = {0};
    creq.width  = width;
    creq.height = height;
    creq.bpp    = 32;
    if (drmIoctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &creq) < 0) {
        perror("DRM_IOCTL_MODE_CREATE_DUMB");
        return 1;
    }

    /* Map the dumb buffer to userspace */
    struct drm_mode_map_dumb mreq = {0};
    mreq.handle = creq.handle;
    if (drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mreq) < 0) {
        perror("DRM_IOCTL_MODE_MAP_DUMB");
        return 1;
    }

    void *map = mmap(0, creq.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, mreq.offset);
    if (map == MAP_FAILED) {
        perror("mmap");
        return 1;
    }

    /* Create a framebuffer object for the dumb buffer */
    uint32_t pitch = creq.pitch;
    uint32_t fb;
    if (drmModeAddFB(fd, width, height, 24, 32, pitch, creq.handle, &fb) < 0) {
        perror("drmModeAddFB");
        return 1;
    }

    /* Fill the buffer: solid red */
    uint32_t *pixels = map;
    for (uint32_t y = 0; y < height; y++) {
        for (uint32_t x = 0; x < width; x++) {
            pixels[y * (pitch / 4) + x] = 0x00FF0000; /* ARGB: red */
        }
    }

    /* Display the buffer */
    drmModeSetCrtc(fd, crtc->crtc_id, fb, 0, 0,
                   &conn->connector_id, 1, &mode);

    /* Keep it on screen for 5 seconds */
    sleep(5);

    /* Cleanup */
    drmModeRmFB(fd, fb);
    munmap(map, creq.size);
    struct drm_mode_destroy_dumb dreq = { .handle = creq.handle };
    drmIoctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &dreq);

    drmModeFreeCrtc(crtc);
    drmModeFreeEncoder(enc);
    drmModeFreeConnector(conn);
    drmModeFreeResources(resources);
    close(fd);

    return 0;
}
