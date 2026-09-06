/* Subset of OpenGL ES 1.x + EGL the game draws with. Every GLfloat
 * argument is really an IEEE-754 bit pattern built by fx_to_f32 — the
 * HLE layer decodes it back to a float host-side. */
#ifndef GLES_H
#define GLES_H

#include "fx.h"

typedef u32 GLenum;
typedef u32 GLuint;
typedef i32 GLint;
typedef u32 GLfloat; /* bit pattern */

/* --- EGL ---------------------------------------------------------------- */
extern GLenum eglGetDisplay(GLuint display_id);
extern GLenum eglInitialize(GLenum dpy, i32* major, i32* minor);
extern GLenum eglChooseConfig(GLenum dpy, const i32* attribs, GLuint* configs, i32 max, i32* num);
extern GLenum eglCreateWindowSurface(GLenum dpy, GLuint cfg, GLuint win, const i32* attribs);
extern GLenum eglCreateContext(GLenum dpy, GLuint cfg, GLuint share, const i32* attribs);
extern GLenum eglMakeCurrent(GLenum dpy, GLuint draw, GLuint read, GLuint ctx);
extern GLenum eglSwapBuffers(GLenum dpy, GLuint surface);
extern GLenum eglTerminate(GLenum dpy);
extern GLenum eglGetError(void);

#define EGL_SURFACE_TYPE 0x3033
#define EGL_WINDOW_BIT 4
#define EGL_BUFFER_SIZE 0x3020
#define EGL_RED_SIZE 0x3024
#define EGL_GREEN_SIZE 0x3023
#define EGL_BLUE_SIZE 0x3022
#define EGL_ALPHA_SIZE 0x3021
#define EGL_DEPTH_SIZE 0x3025
#define EGL_NONE 0x3038

/* --- GL ----------------------------------------------------------------- */
extern void glEnable(GLenum cap);
extern void glDisable(GLenum cap);
extern void glClear(GLenum mask);
extern void glClearColor(GLfloat r, GLfloat g, GLfloat b, GLfloat a);
extern void glMatrixMode(GLenum mode);
extern void glLoadIdentity(void);
extern void glLoadMatrixf(const GLfloat* m);
extern void glFrustumf(GLfloat l, GLfloat r, GLfloat b, GLfloat t, GLfloat n, GLfloat f);
extern void glViewport(GLint x, GLint y, GLint w, GLint h);
extern void glFogf(GLenum pname, GLfloat v);
extern void glFogfv(GLenum pname, const GLfloat* v);
extern void glHint(GLenum target, GLenum mode);
extern void glVertexPointer(GLint size, GLenum type, GLint stride, const void* p);
extern void glColorPointer(GLint size, GLenum type, GLint stride, const void* p);
extern void glTexCoordPointer(GLint size, GLenum type, GLint stride, const void* p);
extern void glEnableClientState(GLenum cap);
extern void glDisableClientState(GLenum cap);
extern void glDrawArrays(GLenum mode, GLint first, GLint count);
extern void glDrawElements(GLenum mode, GLint count, GLenum type, const void* idx);
extern void glBindTexture(GLenum target, GLuint tex);
extern void glGenTextures(GLint n, GLuint* ids);
extern void glTexImage2D(GLenum target, GLint level, GLint internal, GLint w, GLint h,
                         GLint border, GLenum fmt, GLenum type, const void* data);
extern void glTexParameterf(GLenum target, GLenum pname, GLfloat v);
extern void glTexEnvf(GLenum target, GLenum pname, GLfloat v);
extern void glBlendFunc(GLenum sf, GLenum df);
extern void glAlphaFunc(GLenum func, GLfloat ref);
extern void glDepthFunc(GLenum func);
extern void glClearDepthf(GLfloat d);
#define GL_LESS 0x0201
#define GL_LEQUAL 0x0203
extern void glDepthMask(GLuint flag);
extern void glShadeModel(GLenum mode);
extern void glCullFace(GLenum mode);
extern void glColor4f(GLfloat r, GLfloat g, GLfloat b, GLfloat a);
extern GLenum glGetError(void);

#define GL_MODELVIEW 0x1700
#define GL_PROJECTION 0x1701
#define GL_TEXTURE_2D 0x0DE1
#define GL_FLOAT 0x1406
#define GL_UNSIGNED_BYTE 0x1401
#define GL_UNSIGNED_SHORT 0x1403
#define GL_TRIANGLES 0x0004
#define GL_QUADS 0x0007
#define GL_FOG 0x0B60
#define GL_FOG_MODE 0x0B65
#define GL_FOG_COLOR 0x0B66
#define GL_FOG_START 0x0B63
#define GL_FOG_END 0x0B64
#define GL_FOG_LINEAR 0x2601
#define GL_DEPTH_TEST 0x0B71
#define GL_TEXTURE_COORD_ARRAY 0x8078
#define GL_VERTEX_ARRAY 0x8074
#define GL_COLOR_ARRAY 0x8076
#define GL_BLEND 0x0BE2
#define GL_ALPHA_TEST 0x0BC0
#define GL_GREATER 0x0204
#define GL_SRC_ALPHA 0x0302
#define GL_ONE_MINUS_SRC_ALPHA 0x0303
#define GL_COLOR_BUFFER_BIT 0x4000
#define GL_DEPTH_BUFFER_BIT 0x0100
#define GL_SMOOTH 0x1D01
#define GL_FLAT 0x1D00
#define GL_TEXTURE_MIN_FILTER 0x2801
#define GL_TEXTURE_MAG_FILTER 0x2800
#define GL_TEXTURE_WRAP_S 0x2802
#define GL_TEXTURE_WRAP_T 0x2803
#define GL_NEAREST 0x2600
#define GL_REPEAT 0x2901
#define GL_MODULATE 0x2100
#define GL_TEXTURE_ENV 0x2300
#define GL_TEXTURE_ENV_MODE 0x2200
#define GL_BACK 0x0405
#define GL_CULL_FACE 0x0B44
#define GL_PERSPECTIVE_CORRECTION_HINT 0x0C50
#define GL_FASTEST 0x1101
#define GL_NICEST 0x1102
#define GL_PALETTE8_RGBA8_OES 0x8B96

#endif
