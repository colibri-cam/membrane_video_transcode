/* structured_atomic.c – minimal atomic-modeset demo, structured version */

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <errno.h>

#include <drm.h>
#include <drm_fourcc.h>        /* <- for DRM_FORMAT_XRGB8888 */
#include <xf86drm.h>
#include <xf86drmMode.h>

/* ---------- state carried through all steps ---------- */
typedef struct {
    int fd;

    drmModeRes          *res;
    drmModeConnector    *conn;
    drmModeModeInfo      mode;

    uint32_t conn_id;
    uint32_t crtc_id;
    uint32_t plane_id;

    struct drm_mode_create_dumb dumb;
    uint32_t fb_id;
    uint32_t *map;

    uint32_t mode_blob_id;
} drm_state_t;

/* ---------- tiny helpers ---------- */
static void bail(const char *msg)
{
    perror(msg);
    exit(EXIT_FAILURE);
}

static uint32_t crtc_index(drm_state_t *s, uint32_t crtc_id)
{
    for (uint32_t i = 0; i < s->res->count_crtcs; ++i)
        if (s->res->crtcs[i] == crtc_id)
            return i;
    return UINT32_MAX;
}

static uint32_t find_prop(int fd, uint32_t obj_id, uint32_t obj_type,
                          const char *name)
{
    drmModeObjectPropertiesPtr props =
        drmModeObjectGetProperties(fd, obj_id, obj_type);
    if (!props)
        return 0;

    uint32_t prop_id = 0;
    for (uint32_t i = 0; i < props->count_props; ++i) {
        drmModePropertyRes *pr = drmModeGetProperty(fd, props->props[i]);
        if (pr && !strcmp(pr->name, name)) {
            prop_id = pr->prop_id;
            drmModeFreeProperty(pr);
            break;
        }
        drmModeFreeProperty(pr);
    }
    drmModeFreeObjectProperties(props);
    return prop_id;
}

/* ---------- 1. open card & enable atomic ---------- */
static void open_drm(drm_state_t *s, const char *node)
{
    s->fd = open(node, O_RDWR | O_CLOEXEC);
    if (s->fd < 0)
        bail("open drm card");

    if (drmSetClientCap(s->fd, DRM_CLIENT_CAP_ATOMIC, 1))
        bail("DRM_CLIENT_CAP_ATOMIC not supported");
}

/* ---------- 2. pick connector & preferred mode ---------- */
static void pick_connector(drm_state_t *s)
{
    s->res = drmModeGetResources(s->fd);
    if (!s->res)
        bail("drmModeGetResources");

    for (int i = 0; i < s->res->count_connectors; ++i) {
        drmModeConnector *c = drmModeGetConnector(s->fd,
                                                  s->res->connectors[i]);
        if (c && c->connection == DRM_MODE_CONNECTED && c->count_modes) {
            s->conn      = c;
            s->conn_id   = c->connector_id;
            s->mode      = c->modes[0];   /* preferred = first */
            return;
        }
        drmModeFreeConnector(c);
    }
    fprintf(stderr, "No connected connector found\n");
    exit(EXIT_FAILURE);
}

/* ---------- 3. find a CRTC that works with that connector ---------- */
static void pick_crtc(drm_state_t *s)
{
    drmModeEncoder *enc = drmModeGetEncoder(s->fd, s->conn->encoder_id);

    if (enc && enc->crtc_id) {
        s->crtc_id = enc->crtc_id;
    } else if (enc) {
        for (int i = 0; i < s->res->count_crtcs; ++i)
            if (enc->possible_crtcs & (1 << i))
                s->crtc_id = s->res->crtcs[i];
    }
    drmModeFreeEncoder(enc);

    if (!s->crtc_id) {
        fprintf(stderr, "No CRTC found\n");
        exit(EXIT_FAILURE);
    }
}

/* ---------- 4. find the primary plane on that CRTC ---------- */
static void pick_primary_plane(drm_state_t *s)
{
    drmModePlaneRes *pres = drmModeGetPlaneResources(s->fd);
    if (!pres)
        bail("PlaneResources");

    uint32_t wanted = crtc_index(s, s->crtc_id);

    for (uint32_t i = 0; i < pres->count_planes; ++i) {
        drmModePlane *pl = drmModeGetPlane(s->fd, pres->planes[i]);
        if (!pl) continue;

        if (pl->possible_crtcs & (1 << wanted)) {
            drmModeObjectPropertiesPtr op =
                drmModeObjectGetProperties(s->fd,
                                           pl->plane_id,
                                           DRM_MODE_OBJECT_PLANE);
            for (uint32_t j = 0; j < op->count_props; ++j) {
                drmModePropertyRes *pr =
                    drmModeGetProperty(s->fd, op->props[j]);
                if (pr && (pr->flags & DRM_MODE_PROP_IMMUTABLE) &&
                    !strcmp(pr->name, "type") &&
                    op->prop_values[j] == DRM_PLANE_TYPE_PRIMARY) {
                    s->plane_id = pl->plane_id;
                    drmModeFreeProperty(pr);
                    break;
                }
                drmModeFreeProperty(pr);
            }
            drmModeFreeObjectProperties(op);
        }
        drmModeFreePlane(pl);
        if (s->plane_id) break;
    }
    drmModeFreePlaneResources(pres);

    if (!s->plane_id) {
        fprintf(stderr, "Primary plane not found\n");
        exit(EXIT_FAILURE);
    }
}

/* ---------- 5. create dumb buffer + FB, map it ---------- */
static void create_fb(drm_state_t *s)
{
    memset(&s->dumb, 0, sizeof(s->dumb));
    s->dumb.width  = s->mode.hdisplay;
    s->dumb.height = s->mode.vdisplay;
    s->dumb.bpp    = 32;

    if (drmIoctl(s->fd, DRM_IOCTL_MODE_CREATE_DUMB, &s->dumb) < 0)
        bail("CREATE_DUMB");

    if (drmModeAddFB2(s->fd,
                      s->dumb.width, s->dumb.height,
                      DRM_FORMAT_XRGB8888,
                      (uint32_t[4]){ s->dumb.handle },
                      (uint32_t[4]){ s->dumb.pitch },
                      (uint32_t[4]){ 0 },          /* <- offsets is uint32_t */
                      &s->fb_id, 0))
        bail("AddFB2");

    struct drm_mode_map_dumb m = { .handle = s->dumb.handle };
    if (drmIoctl(s->fd, DRM_IOCTL_MODE_MAP_DUMB, &m) < 0)
        bail("MAP_DUMB");

    s->map = mmap(NULL, s->dumb.size, PROT_READ | PROT_WRITE,
                  MAP_SHARED, s->fd, m.offset);
    if (s->map == MAP_FAILED)
        bail("mmap");
}

/* ---------- 6. paint simple RGB bars ---------- */
static void paint_colorbars(drm_state_t *s)
{
    uint32_t stride32 = s->dumb.pitch / 4;
    for (uint32_t y = 0; y < s->dumb.height; ++y) {
        for (uint32_t x = 0; x < s->dumb.width; ++x) {
            uint8_t bar = x * 3 / s->dumb.width;
            uint32_t pix = (bar == 0) ? 0x00ff0000 :
                           (bar == 1) ? 0x0000ff00 :
                                         0x000000ff;
            s->map[y * stride32 + x] = pix;
        }
    }
}

/* helper to add a plane property */
static void add_plane_prop(drm_state_t *s, drmModeAtomicReq *req,
                           const char *name, uint64_t val)
{
    uint32_t pid = find_prop(s->fd, s->plane_id,
                             DRM_MODE_OBJECT_PLANE, name);
    drmModeAtomicAddProperty(req, s->plane_id, pid, val);
}

/* ---------- 7. atomic commit ---------- */
static void atomic_commit(drm_state_t *s)
{
    drmModeAtomicReq *req = drmModeAtomicAlloc();
    if (!req)
        bail("AtomicAlloc");

    /* connector -> CRTC */
    drmModeAtomicAddProperty(req, s->conn_id,
        find_prop(s->fd, s->conn_id, DRM_MODE_OBJECT_CONNECTOR, "CRTC_ID"),
        s->crtc_id);

    /* CRTC MODE_ID / ACTIVE */
    uint32_t pid_mode   = find_prop(s->fd, s->crtc_id,
                                    DRM_MODE_OBJECT_CRTC, "MODE_ID");
    uint32_t pid_active = find_prop(s->fd, s->crtc_id,
                                    DRM_MODE_OBJECT_CRTC, "ACTIVE");

    if (drmModeCreatePropertyBlob(s->fd, &s->mode,
                                  sizeof(s->mode), &s->mode_blob_id))
        bail("CreatePropertyBlob");

    drmModeAtomicAddProperty(req, s->crtc_id, pid_mode,   s->mode_blob_id);
    drmModeAtomicAddProperty(req, s->crtc_id, pid_active, 1);

    /* plane coordinates */
    add_plane_prop(s, req, "FB_ID",   s->fb_id);
    add_plane_prop(s, req, "CRTC_ID", s->crtc_id);
    add_plane_prop(s, req, "SRC_X",   0);
    add_plane_prop(s, req, "SRC_Y",   0);
    add_plane_prop(s, req, "SRC_W",   (uint64_t)s->mode.hdisplay << 16);
    add_plane_prop(s, req, "SRC_H",   (uint64_t)s->mode.vdisplay << 16);
    add_plane_prop(s, req, "CRTC_X",  0);
    add_plane_prop(s, req, "CRTC_Y",  0);
    add_plane_prop(s, req, "CRTC_W",  s->mode.hdisplay);
    add_plane_prop(s, req, "CRTC_H",  s->mode.vdisplay);

    if (drmModeAtomicCommit(s->fd, req,
            DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_ALLOW_MODESET,
            NULL) < 0)
        bail("AtomicCommit");

    drmModeAtomicFree(req);
}

/* ---------- 8. cleanup ---------- */
static void cleanup(drm_state_t *s)
{
    drmModeDestroyPropertyBlob(s->fd, s->mode_blob_id);
    munmap(s->map, s->dumb.size);
    drmModeRmFB(s->fd, s->fb_id);

    struct drm_mode_destroy_dumb d = { .handle = s->dumb.handle };
    drmIoctl(s->fd, DRM_IOCTL_MODE_DESTROY_DUMB, &d);

    drmModeFreeConnector(s->conn);
    drmModeFreeResources(s->res);
    close(s->fd);
}

/* ---------- main orchestration ---------- */
int main(int argc, char **argv)
{
    const char *node = (argc > 1) ? argv[1] : "/dev/dri/card0";

    drm_state_t s = {0};

    open_drm(&s, node);
    pick_connector(&s);
    pick_crtc(&s);
    pick_primary_plane(&s);
    create_fb(&s);
    paint_colorbars(&s);
    atomic_commit(&s);

    printf("Displayed for 5 seconds on %s\n", node);
    sleep(5);

    cleanup(&s);
    return 0;
}
