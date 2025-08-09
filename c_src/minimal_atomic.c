/* structured_atomic.c – minimal atomic demo that scrolls 1 px per vsync */

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
#include <sys/select.h>

#include <drm.h>
#include <drm_fourcc.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

/* ---------- tiny helpers ---------- */
#define DIE(msg)  do { perror(msg); exit(EXIT_FAILURE); } while (0)

/* ------------------------------------------------------------------ */
/* Fallback for libdrm < 2.4.115 – drop-in replacement                */
#ifndef HAVE_DRM_MODE_GET_CRTC_INDEX     /* header doesn't define it? */
static inline int drmModeGetCrtcIndex(const drmModeRes *res, uint32_t crtc_id)
{
    for (int i = 0; i < res->count_crtcs; ++i)
        if (res->crtcs[i] == crtc_id)
            return i;
    return -1;   /* not found */
}
#endif
/* ------------------------------------------------------------------ */

static uint32_t find_prop(int fd, uint32_t obj_id, uint32_t obj_type,
                          const char *name)
{
    drmModeObjectPropertiesPtr p = drmModeObjectGetProperties(fd, obj_id, obj_type);
    if (!p) return 0;
    uint32_t id = 0;
    for (uint32_t i = 0; i < p->count_props; ++i) {
        drmModePropertyRes *pr = drmModeGetProperty(fd, p->props[i]);
        if (pr && !strcmp(pr->name, name)) { id = pr->prop_id; }
        drmModeFreeProperty(pr);
        if (id) break;
    }
    drmModeFreeObjectProperties(p);
    return id;
}

/* ---------- state ---------- */
typedef struct {
    int fd;
    drmModeRes       *res;
    drmModeConnector *conn;
    drmModeModeInfo   mode;
    uint32_t conn_id, crtc_id, plane_id;

    struct drm_mode_create_dumb dumb;
    uint32_t fb_id;
    uint32_t *map;
    uint32_t mode_blob;
} drm_state_t;

/* ---------- opening / probing (unchanged) ---------- */
static void open_card(drm_state_t *s, const char *node)
{
    if ((s->fd = open(node, O_RDWR | O_CLOEXEC)) < 0) DIE("open");
    if (drmSetClientCap(s->fd, DRM_CLIENT_CAP_ATOMIC, 1)) DIE("atomic-cap");
}

static void probe(drm_state_t *s)
{
    /* pick connector / mode */
    if (!(s->res = drmModeGetResources(s->fd))) DIE("GetResources");
    for (int i = 0; i < s->res->count_connectors; ++i) {
        drmModeConnector *c = drmModeGetConnector(s->fd, s->res->connectors[i]);
        if (c && c->connection == DRM_MODE_CONNECTED && c->count_modes) {
            s->conn = c;  s->conn_id = c->connector_id;  s->mode = c->modes[0]; break;
        }
        drmModeFreeConnector(c);
    }
    if (!s->conn) { fprintf(stderr,"no connector\n"); exit(1); }

    /* pick CRTC */
    drmModeEncoder *enc = drmModeGetEncoder(s->fd, s->conn->encoder_id);
    if (enc && enc->crtc_id) s->crtc_id = enc->crtc_id;
    else if (enc) {
        for (int i=0;i<s->res->count_crtcs;++i)
            if (enc->possible_crtcs&(1<<i)) s->crtc_id = s->res->crtcs[i];
    }
    drmModeFreeEncoder(enc);
    if (!s->crtc_id) { fprintf(stderr,"no crtc\n"); exit(1); }

    /* pick primary plane */
    drmModePlaneRes *pr = drmModeGetPlaneResources(s->fd);
    for (uint32_t i=0;i<pr->count_planes;++i){
        drmModePlane *pl = drmModeGetPlane(s->fd, pr->planes[i]);
        if ((pl->possible_crtcs&(1<<drmModeGetCrtcIndex(s->res,s->crtc_id)))){
            drmModeObjectPropertiesPtr op =
                drmModeObjectGetProperties(s->fd, pl->plane_id, DRM_MODE_OBJECT_PLANE);
            for (uint32_t j=0;j<op->count_props;++j){
                drmModePropertyRes *p = drmModeGetProperty(s->fd,op->props[j]);
                if (p && (p->flags&DRM_MODE_PROP_IMMUTABLE) &&
                    !strcmp(p->name,"type") && op->prop_values[j]==DRM_PLANE_TYPE_PRIMARY){
                    s->plane_id = pl->plane_id;
                }
                drmModeFreeProperty(p);
            }
            drmModeFreeObjectProperties(op);
        }
        drmModeFreePlane(pl);
        if (s->plane_id) break;
    }
    drmModeFreePlaneResources(pr);
    if (!s->plane_id){ fprintf(stderr,"no plane\n"); exit(1); }
}

/* ---------- dumb buffer / FB (unchanged) ---------- */
static void make_fb(drm_state_t *s)
{
    s->dumb.width  = s->mode.hdisplay;
    s->dumb.height = s->mode.vdisplay;
    s->dumb.bpp    = 32;
    if (drmIoctl(s->fd, DRM_IOCTL_MODE_CREATE_DUMB, &s->dumb)) DIE("CREATE_DUMB");
    if (drmModeAddFB2(s->fd, s->dumb.width, s->dumb.height, DRM_FORMAT_XRGB8888,
                      (uint32_t[4]){s->dumb.handle},
                      (uint32_t[4]){s->dumb.pitch},
                      (uint32_t[4]){0}, &s->fb_id, 0)) DIE("AddFB2");
    struct drm_mode_map_dumb m = { .handle = s->dumb.handle };
    if (drmIoctl(s->fd, DRM_IOCTL_MODE_MAP_DUMB, &m)) DIE("MAP_DUMB");
    s->map = mmap(0,s->dumb.size,PROT_READ|PROT_WRITE,MAP_SHARED,s->fd,m.offset);
    if (s->map==MAP_FAILED) DIE("mmap");
}

/* ---------- paint bars (unchanged) ---------- */
static void paint(drm_state_t *s)
{
    uint32_t stride=s->dumb.pitch/4,w=s->dumb.width,h=s->dumb.height;
    for(uint32_t y=0;y<h;++y)for(uint32_t x=0;x<w;++x){
        uint8_t b=x*3/w;
        uint32_t pix=b==0?0xff0000:b==1?0x00ff00:0x0000ff;
        s->map[y*stride+x]=pix;
    }
}

/* ---------- initial modeset ---------- */
static void first_commit(drm_state_t *s)
{
    drmModeAtomicReq *req=drmModeAtomicAlloc();
    drmModeCreatePropertyBlob(s->fd,&s->mode,sizeof(s->mode),&s->mode_blob);

    drmModeAtomicAddProperty(req,s->conn_id,
        find_prop(s->fd,s->conn_id,DRM_MODE_OBJECT_CONNECTOR,"CRTC_ID"),s->crtc_id);

    uint32_t mid=find_prop(s->fd,s->crtc_id,DRM_MODE_OBJECT_CRTC,"MODE_ID");
    uint32_t act=find_prop(s->fd,s->crtc_id,DRM_MODE_OBJECT_CRTC,"ACTIVE");
    drmModeAtomicAddProperty(req,s->crtc_id,mid,s->mode_blob);
    drmModeAtomicAddProperty(req,s->crtc_id,act,1);

    uint32_t fx=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"FB_ID");
    uint32_t cx=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"CRTC_ID");
    uint32_t sx=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"SRC_X");
    uint32_t sy=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"SRC_Y");
    uint32_t sw=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"SRC_W");
    uint32_t sh=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"SRC_H");
    uint32_t crx=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"CRTC_X");
    uint32_t cry=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"CRTC_Y");
    uint32_t crw=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"CRTC_W");
    uint32_t crh=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"CRTC_H");

    drmModeAtomicAddProperty(req,s->plane_id,fx,s->fb_id);
    drmModeAtomicAddProperty(req,s->plane_id,cx,s->crtc_id);
    drmModeAtomicAddProperty(req,s->plane_id,sx,0);
    drmModeAtomicAddProperty(req,s->plane_id,sy,0);
    drmModeAtomicAddProperty(req,s->plane_id,sw,(uint64_t)s->mode.hdisplay<<16);
    drmModeAtomicAddProperty(req,s->plane_id,sh,(uint64_t)s->mode.vdisplay<<16);
    drmModeAtomicAddProperty(req,s->plane_id,crx,0);
    drmModeAtomicAddProperty(req,s->plane_id,cry,0);
    drmModeAtomicAddProperty(req,s->plane_id,crw,s->mode.hdisplay);
    drmModeAtomicAddProperty(req,s->plane_id,crh,s->mode.vdisplay);

    if (drmModeAtomicCommit(s->fd,req,DRM_MODE_ATOMIC_ALLOW_MODESET,0))
        DIE("first commit");
    drmModeAtomicFree(req);
}

/* ---------- per-frame update ---------- */
static void update_plane_x(drm_state_t *s,uint32_t x)
{
    drmModeAtomicReq *req = drmModeAtomicAlloc();
    uint32_t crx=find_prop(s->fd,s->plane_id,DRM_MODE_OBJECT_PLANE,"CRTC_X");
    drmModeAtomicAddProperty(req,s->plane_id,crx,x);
    if (drmModeAtomicCommit(s->fd,req,
            DRM_MODE_ATOMIC_NONBLOCK|DRM_MODE_PAGE_FLIP_EVENT,0))
        DIE("commit");
    drmModeAtomicFree(req);
}

/* ---------- event handling & animation loop ---------- */
static volatile bool flip_done=true;
static void page_flip_handler(int fd,unsigned int frame,
                              unsigned int sec,unsigned int usec,void *data)
{ (void)fd;(void)frame;(void)sec;(void)usec; flip_done=true; }

static void animate(drm_state_t *s)
{
    drmEventContext ev = { .version = DRM_EVENT_CONTEXT_VERSION,
                           .page_flip_handler = page_flip_handler };

    uint32_t xpos = 0, max = s->mode.hdisplay;

    flip_done = false;          /* <-- tell loop we’re now waiting */
    update_plane_x(s, xpos);    /* queue first NONBLOCK commit     */

    while (1) {
        /* wait until page-flip handler sets flip_done = true */
        while (!flip_done) {
            fd_set fds; FD_ZERO(&fds); FD_SET(s->fd, &fds);
            if (select(s->fd + 1, &fds, NULL, NULL, NULL) < 0)
                DIE("select");
            if (FD_ISSET(s->fd, &fds))
                drmHandleEvent(s->fd, &ev);
        }
        flip_done = false;      /* arm for the NEXT event          */

        xpos = (xpos + 1) % max;
        update_plane_x(s, xpos); /* queues next frame              */
    }
}

/* ---------- cleanup ---------- */
static void cleanup(drm_state_t *s)
{
    drmModeDestroyPropertyBlob(s->fd,s->mode_blob);
    munmap(s->map,s->dumb.size);
    drmModeRmFB(s->fd,s->fb_id);
    struct drm_mode_destroy_dumb d={.handle=s->dumb.handle};
    drmIoctl(s->fd,DRM_IOCTL_MODE_DESTROY_DUMB,&d);
    drmModeFreeConnector(s->conn);
    drmModeFreeResources(s->res);
    close(s->fd);
}

/* ---------- main ---------- */
int main(int argc,char **argv)
{
    const char *node=(argc>1)?argv[1]:"/dev/dri/card0";
    drm_state_t s={0};

    open_card(&s,node);
    probe(&s);
    make_fb(&s);
    paint(&s);
    first_commit(&s);

    printf("scrolling … Ctrl-C to quit\n");
    animate(&s);

    cleanup(&s);
    return 0;
}
