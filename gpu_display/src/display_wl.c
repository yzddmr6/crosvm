// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <assert.h>
#include <memory.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include "aura-shell.h"
#include "linux-dmabuf-unstable-v1.h"
#include "viewporter.h"
#include "xdg-shell.h"
#include "virtio-gpu-metadata-v1.h"
#include "xdg-decoration-unstable-v1.h"
#include <wayland-client-core.h>
#include <wayland-client-protocol.h>
#include <wayland-client.h>

// BTN_LEFT is copied from linux/input-event-codes.h because the kernel headers
// aren't readily available in some downstream projects.
#define BTN_LEFT 0x110

#define DEFAULT_SCALE 2
#define MAX_BUFFER_COUNT 64
#define EVENT_BUF_SIZE 256

const int32_t DWL_KEYBOARD_KEY_STATE_RELEASED = WL_KEYBOARD_KEY_STATE_RELEASED;
const int32_t DWL_KEYBOARD_KEY_STATE_PRESSED  = WL_KEYBOARD_KEY_STATE_PRESSED;

const uint32_t DWL_EVENT_TYPE_KEYBOARD_ENTER = 0x00;
const uint32_t DWL_EVENT_TYPE_KEYBOARD_LEAVE = 0x01;
const uint32_t DWL_EVENT_TYPE_KEYBOARD_KEY   = 0x02;
const uint32_t DWL_EVENT_TYPE_POINTER_ENTER  = 0x10;
const uint32_t DWL_EVENT_TYPE_POINTER_LEAVE  = 0x11;
const uint32_t DWL_EVENT_TYPE_POINTER_MOVE   = 0x12;
const uint32_t DWL_EVENT_TYPE_POINTER_BUTTON = 0x13;
const uint32_t DWL_EVENT_TYPE_TOUCH_DOWN     = 0x20;
const uint32_t DWL_EVENT_TYPE_TOUCH_UP       = 0x21;
const uint32_t DWL_EVENT_TYPE_TOUCH_MOTION   = 0x22;
/* iceland gvm (input modes, 0.2-6): raw pointer + real multi-finger touch; the mode logic is gvm_input.rs */
const uint32_t DWL_EVENT_TYPE_POINTER_AXIS   = 0x14;
const uint32_t DWL_EVENT_TYPE_FINGER_DOWN    = 0x30;
const uint32_t DWL_EVENT_TYPE_FINGER_UP      = 0x31;
const uint32_t DWL_EVENT_TYPE_FINGER_MOTION  = 0x32;
const uint32_t DWL_EVENT_TYPE_FINGER_CANCEL  = 0x33;

const uint32_t DWL_SURFACE_FLAG_RECEIVE_INPUT = 1 << 0;
const uint32_t DWL_SURFACE_FLAG_HAS_ALPHA     = 1 << 1;

struct dwl_event {
	const void *surface_descriptor;
	uint32_t event_type;
	int32_t params[3];
};

struct dwl_context;

struct interfaces {
	struct dwl_context *context;
	struct wl_compositor *compositor;
	struct wl_subcompositor *subcompositor;
	struct wl_shm *shm;
	struct wl_seat *seat;
	struct zaura_shell *aura; // optional
	struct zwp_linux_dmabuf_v1 *linux_dmabuf;
	struct xdg_wm_base *xdg_wm_base;
	struct wp_viewporter *viewporter; // optional
	struct wp_virtio_gpu_metadata_v1 *virtio_gpu_metadata; // optional
	struct zxdg_decoration_manager_v1 *decoration_manager; /* iceland gvm: KWin server-side frame */
};

struct output {
	struct wl_output *output;
	struct zaura_output *aura_output;
	struct dwl_context *context;
	uint32_t id;
	uint32_t current_scale;
	uint32_t device_scale_factor;
	bool internal;
	char name[64]; /* iceland gvm: wl_output v4 name (e.g. "DSI-1", "DP-1") for CROSVM_GVM_OUTPUT */
};

struct input {
	struct wl_keyboard *wl_keyboard;
	struct wl_pointer *wl_pointer;
	struct wl_surface *keyboard_input_surface;
	struct wl_surface *pointer_input_surface;
	int32_t pointer_x;
	int32_t pointer_y;
	bool pointer_lbutton_state;
	struct wl_touch *wl_touch;       /* iceland gvm */
	struct wl_surface *touch_surface;
};

typedef void (*dwl_error_callback_type)(const char *message);

struct dwl_context {
	struct wl_display *display;
	struct dwl_surface *surfaces[MAX_BUFFER_COUNT];
	struct dwl_dmabuf *dmabufs[MAX_BUFFER_COUNT];
	struct interfaces ifaces;
	struct input input;
	bool output_added;
	struct output outputs[8];

	struct dwl_event event_cbuf[EVENT_BUF_SIZE];
	size_t event_read_pos;
	size_t event_write_pos;

	dwl_error_callback_type error_callback;
};

#define outputs_for_each(context, pos, output)                                 \
	for (pos = 0, output = &context->outputs[pos];                         \
	     pos < (sizeof(context->outputs) / sizeof(context->outputs[0]));   \
	     pos++, output = &context->outputs[pos])

struct dwl_dmabuf {
	uint32_t width;
	uint32_t height;
	uint32_t import_id;
	bool in_use;
	struct wl_buffer *buffer;
	struct dwl_context *context;
};

struct dwl_surface {
	struct dwl_context *context;
	struct wl_surface *wl_surface;
	struct zaura_surface *aura;
	struct xdg_surface *xdg_surface;
	struct xdg_toplevel *xdg_toplevel;
	struct zxdg_toplevel_decoration_v1 *decoration;
	struct wp_viewport *viewport;
	struct wp_virtio_gpu_surface_metadata_v1 *virtio_gpu_surface_metadata;
	struct wl_subsurface *subsurface;
	uint32_t width;
	uint32_t height;
	uint32_t surface_id;
	double scale;
	/* iceland gvm (0.2-6): the compositor's fullscreen size (logical px; 0 = not fullscreen). The buffer is
	 * then scaled to it with the viewport and input is mapped back per axis. */
	int32_t fs_width;
	int32_t fs_height;
	bool gvm_commit;
	bool close_requested;
	size_t buffer_count;
	uint64_t buffer_use_bit_mask;
	struct wl_buffer *buffers[0];
};

static_assert(sizeof(((struct dwl_surface *)0)->buffer_use_bit_mask) * 8 >=
		  MAX_BUFFER_COUNT,
	      "not enough bits in buffer_use_bit_mask");

static void output_geometry(void *data, struct wl_output *output, int x, int y,
			    int physical_width, int physical_height,
			    int subpixel, const char *make, const char *model,
			    int transform)
{
	(void)data;
	(void)output;
	(void)x;
	(void)y;
	(void)physical_width;
	(void)physical_height;
	(void)subpixel;
	(void)make;
	(void)model;
	(void)transform;
}

static void output_mode(void *data, struct wl_output *output, uint32_t flags,
			int width, int height, int refresh)
{
	(void)data;
	(void)output;
	(void)flags;
	(void)width;
	(void)height;
	(void)refresh;
}

static void output_done(void *data, struct wl_output *output)
{
	(void)data;
	(void)output;
}

static void output_scale(void *data, struct wl_output *wl_output,
			 int32_t scale_factor)
{
	(void)wl_output;
	struct output *output = (struct output *)data;
	struct dwl_context *context = output->context;

	// If the aura interface is available, we prefer the scale factor
	// reported by that.
	if (context->ifaces.aura)
		return;

	output->current_scale = 1000 * scale_factor;
}

static void output_name(void *data, struct wl_output *wl_output, const char *name)
{
	(void)wl_output;
	struct output *output = (struct output *)data;
	snprintf(output->name, sizeof(output->name), "%s", name ? name : "");
}

static void output_description(void *data, struct wl_output *wl_output, const char *description)
{
	(void)data;
	(void)wl_output;
	(void)description;
}

static const struct wl_output_listener output_listener = {
    .geometry = output_geometry,
    .mode = output_mode,
    .done = output_done,
    .scale = output_scale,
    .name = output_name,
    .description = output_description};

static void aura_output_scale(void *data, struct zaura_output *aura_output,
			      uint32_t flags, uint32_t scale)
{
	(void)aura_output;
	struct output *output = (struct output *)data;
	if (flags & ZAURA_OUTPUT_SCALE_PROPERTY_CURRENT) {
		output->current_scale = scale;
	}
}

static void aura_output_connection(void *data, struct zaura_output *aura_output,
				   uint32_t connection)
{
	(void)aura_output;
	struct output *output = (struct output *)data;
	output->internal = connection == ZAURA_OUTPUT_CONNECTION_TYPE_INTERNAL;
}

static void aura_output_device_scale_factor(void *data,
					    struct zaura_output *aura_output,
					    uint32_t device_scale_factor)
{
	(void)aura_output;
	struct output *output = (struct output *)data;
	output->device_scale_factor = device_scale_factor;
}

static const struct zaura_output_listener aura_output_listener = {
    .scale = aura_output_scale,
    .connection = aura_output_connection,
    .device_scale_factor = aura_output_device_scale_factor};

static void xdg_wm_base_ping(void *data, struct xdg_wm_base *xdg_wm_base,
			     uint32_t serial)
{
	(void)data;
	xdg_wm_base_pong(xdg_wm_base, serial);
}

static const struct xdg_wm_base_listener xdg_wm_base_listener = {
	.ping = xdg_wm_base_ping,
};


static void wl_keyboard_keymap(void *data, struct wl_keyboard *wl_keyboard,
			       uint32_t format, int32_t fd, uint32_t size)
{
	struct dwl_context *context = (struct dwl_context*)data;
	(void)wl_keyboard;
	(void)fd;
	(void)size;
	if (format != WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1) {
		context->error_callback("wl_keyboard: invalid keymap format");
	}
}

static void dwl_context_push_event(struct dwl_context *self,
				   struct dwl_event *event)
{
	if (!self)
		return;

	memcpy(self->event_cbuf + self->event_write_pos, event,
	       sizeof(struct dwl_event));

	if (++self->event_write_pos == EVENT_BUF_SIZE)
		self->event_write_pos = 0;
}

static void wl_keyboard_enter(void *data, struct wl_keyboard *wl_keyboard,
			      uint32_t serial, struct wl_surface *surface,
			      struct wl_array *keys)
{
	(void)wl_keyboard;
	(void)serial;
	(void)surface;
	struct dwl_context *context = (struct dwl_context*)data;
	struct input *input = &context->input;
	uint32_t *key;
	struct dwl_event event = {0};
	input->keyboard_input_surface = surface;
	wl_array_for_each(key, keys) {
		event.surface_descriptor = input->keyboard_input_surface;
		event.event_type = DWL_EVENT_TYPE_KEYBOARD_KEY;
		event.params[0] = (int32_t)*key;
		event.params[1] = DWL_KEYBOARD_KEY_STATE_PRESSED;
		dwl_context_push_event(context, &event);
	}
}

static void wl_keyboard_key(void *data, struct wl_keyboard *wl_keyboard,
			    uint32_t serial, uint32_t time, uint32_t key,
			    uint32_t state)
{
	struct dwl_context *context = (struct dwl_context*)data;
	struct input *input = &context->input;
	(void)wl_keyboard;
	(void)serial;
	(void)time;
	struct dwl_event event = {0};
	event.surface_descriptor = input->keyboard_input_surface;
	event.event_type = DWL_EVENT_TYPE_KEYBOARD_KEY;
	event.params[0] = (int32_t)key;
	event.params[1] = state;
	dwl_context_push_event(context, &event);
}

static void wl_keyboard_leave(void *data, struct wl_keyboard *wl_keyboard,
			      uint32_t serial, struct wl_surface *surface)
{
	struct dwl_context *context = (struct dwl_context*)data;
	struct input *input = &context->input;
	struct dwl_event event = {0};
	(void)wl_keyboard;
	(void)serial;
	(void)surface;

	event.surface_descriptor = input->keyboard_input_surface;
	event.event_type = DWL_EVENT_TYPE_KEYBOARD_LEAVE;
	dwl_context_push_event(context, &event);

	input->keyboard_input_surface = NULL;
}

static void wl_keyboard_modifiers(void *data, struct wl_keyboard *wl_keyboard,
				  uint32_t serial, uint32_t mods_depressed,
				  uint32_t mods_latched, uint32_t mods_locked,
				  uint32_t group)
{
	(void)data;
	(void)wl_keyboard;
	(void)serial;
	(void)mods_depressed;
	(void)mods_latched;
	(void)mods_locked;
	(void)group;
}

static void wl_keyboard_repeat_info(void *data, struct wl_keyboard *wl_keyboard,
				    int32_t rate, int32_t delay)
{
	(void)data;
	(void)wl_keyboard;
	(void)rate;
	(void)delay;
}

static const struct wl_keyboard_listener wl_keyboard_listener = {
	.keymap = wl_keyboard_keymap,
	.enter = wl_keyboard_enter,
	.leave = wl_keyboard_leave,
	.key = wl_keyboard_key,
	.modifiers = wl_keyboard_modifiers,
	.repeat_info = wl_keyboard_repeat_info,
};

/* iceland gvm: surface (logical) coordinates -> buffer pixels, the guest devices' coordinate space
 * (up to v10i the window's logical coordinates went to a device sized in buffer pixels) */
static int32_t gvm_to_buffer_axis(struct wl_surface *surface, wl_fixed_t v, int axis)
{
	double scale = 1.0;
	if (surface) {
		struct dwl_surface *ds = (struct dwl_surface *)wl_surface_get_user_data(surface);
		if (ds && ds->fs_width > 0 && ds->fs_height > 0)
			/* fullscreen: the viewport stretches the buffer to the compositor's size */
			scale = axis == 0 ? (double)ds->width / ds->fs_width : (double)ds->height / ds->fs_height;
		else if (ds && ds->scale > 0)
			scale = ds->scale;
	}
	return (int32_t)(wl_fixed_to_double(v) * scale);
}
#define gvm_to_buffer_x(s, v) gvm_to_buffer_axis((s), (v), 0)
#define gvm_to_buffer_y(s, v) gvm_to_buffer_axis((s), (v), 1)

static void gvm_push(struct dwl_context *context, const void *surface, uint32_t type,
		     int32_t p0, int32_t p1, int32_t p2)
{
	struct dwl_event event = {0};
	event.surface_descriptor = surface;
	event.event_type = type;
	event.params[0] = p0;
	event.params[1] = p1;
	event.params[2] = p2;
	dwl_context_push_event(context, &event);
}

static void pointer_enter_handler(void *data, struct wl_pointer *wl_pointer,
				  uint32_t serial, struct wl_surface *surface,
				  wl_fixed_t x, wl_fixed_t y)
{
	struct dwl_context *context = (struct dwl_context*)data;
	struct input *input = &context->input;
	(void)wl_pointer;
	(void)serial;

	input->pointer_input_surface = surface;
	input->pointer_x = gvm_to_buffer_x(surface, x);
	input->pointer_y = gvm_to_buffer_y(surface, y);
	gvm_push(context, surface, DWL_EVENT_TYPE_POINTER_MOVE, input->pointer_x, input->pointer_y, 0);
}

static void pointer_leave_handler(void *data, struct wl_pointer *wl_pointer,
				  uint32_t serial, struct wl_surface *surface)
{
	struct dwl_context *context = (struct dwl_context*)data;
	struct input *input = &context->input;
	(void)wl_pointer;
	(void)serial;
	(void)surface;

	input->pointer_input_surface = NULL;
}

static void pointer_motion_handler(void *data, struct wl_pointer *wl_pointer,
				   uint32_t time, wl_fixed_t x, wl_fixed_t y)
{
	struct dwl_context *context = (struct dwl_context*)data;
	struct input *input = &context->input;
	(void)wl_pointer;
	(void)time;

	if (!input->pointer_input_surface)
		return;
	input->pointer_x = gvm_to_buffer_x(input->pointer_input_surface, x);
	input->pointer_y = gvm_to_buffer_y(input->pointer_input_surface, y);
	gvm_push(context, input->pointer_input_surface, DWL_EVENT_TYPE_POINTER_MOVE,
		 input->pointer_x, input->pointer_y, 0);
}

static void pointer_button_handler(void *data, struct wl_pointer *wl_pointer,
				   uint32_t serial, uint32_t time, uint32_t button,
				   uint32_t state)
{
	struct dwl_context *context = (struct dwl_context*)data;
	struct input *input = &context->input;
	(void)wl_pointer;
	(void)time;
	(void)serial;

	if (!input->pointer_input_surface)
		return;
	if (button == BTN_LEFT)
		input->pointer_lbutton_state = state != 0;
	/* every button; the touch mode (gvm_input.rs) still turns the left one into a touch */
	gvm_push(context, input->pointer_input_surface, DWL_EVENT_TYPE_POINTER_BUTTON,
		 (int32_t)button, state != 0, 0);
}

static void wl_pointer_frame(void *data, struct wl_pointer *wl_pointer)
{
	(void)data;
	(void)wl_pointer;
}

static void pointer_axis_handler(void *data, struct wl_pointer *wl_pointer,
				 uint32_t time, uint32_t axis, wl_fixed_t value)
{
	struct dwl_context *context = (struct dwl_context*)data;
	struct input *input = &context->input;
	(void)wl_pointer;
	(void)time;

	if (!input->pointer_input_surface)
		return;
	/* params[1] = wl_fixed (value * 256); gvm_input.rs turns it into wheel notches */
	gvm_push(context, input->pointer_input_surface, DWL_EVENT_TYPE_POINTER_AXIS, (int32_t)axis, value, 0);
}

static void wl_pointer_axis_source(void *data, struct wl_pointer *wl_pointer,
				   uint32_t axis_source)
{
	(void)data;
	(void)wl_pointer;
	(void)axis_source;
}

static void wl_pointer_axis_stop(void *data, struct wl_pointer *wl_pointer,
					uint32_t time, uint32_t axis)
{
	(void)data;
	(void)wl_pointer;
	(void)time;
	(void)axis;
}

static void wl_pointer_axis_discrete(void *data, struct wl_pointer *wl_pointer,
				     uint32_t axis, int32_t discrete)
{
	(void)data;
	(void)wl_pointer;
	(void)axis;
	(void)discrete;
}

const struct wl_pointer_listener wl_pointer_listener = {
	.enter = pointer_enter_handler,
	.leave = pointer_leave_handler,
	.motion = pointer_motion_handler,
	.button = pointer_button_handler,
	.axis = pointer_axis_handler,
	.frame = wl_pointer_frame,
	.axis_source = wl_pointer_axis_source,
	.axis_stop = wl_pointer_axis_stop,
	.axis_discrete = wl_pointer_axis_discrete,
};

/* iceland gvm: real multi-finger touch (KWin sends wl_touch to clients that bind it) */
static void touch_down(void *data, struct wl_touch *wl_touch, uint32_t serial, uint32_t time,
		       struct wl_surface *surface, int32_t id, wl_fixed_t x, wl_fixed_t y)
{
	struct dwl_context *context = (struct dwl_context*)data;
	(void)wl_touch;
	(void)serial;
	(void)time;
	context->input.touch_surface = surface;
	gvm_push(context, surface, DWL_EVENT_TYPE_FINGER_DOWN, id, gvm_to_buffer_x(surface, x),
		 gvm_to_buffer_y(surface, y));
	/* iceland gvm (p1 VIN): CROSVM_GVM_INPUT_LOG=1 logs the first 400 finger downs (surface -> buffer) */
	{
		static int n;
		const char *lg = getenv("CROSVM_GVM_INPUT_LOG");
		if (lg && *lg == '1' && n++ < 400) {
			fprintf(stderr, "GVM-TOUCH down id=%d surface=%.1f,%.1f buffer=%d,%d\n", id,
				wl_fixed_to_double(x), wl_fixed_to_double(y), gvm_to_buffer_x(surface, x),
				gvm_to_buffer_y(surface, y));
			fflush(stderr);
		}
	}
}

static void touch_up(void *data, struct wl_touch *wl_touch, uint32_t serial, uint32_t time, int32_t id)
{
	struct dwl_context *context = (struct dwl_context*)data;
	(void)wl_touch;
	(void)serial;
	(void)time;
	if (context->input.touch_surface)
		gvm_push(context, context->input.touch_surface, DWL_EVENT_TYPE_FINGER_UP, id, 0, 0);
}

static void touch_motion(void *data, struct wl_touch *wl_touch, uint32_t time, int32_t id,
			 wl_fixed_t x, wl_fixed_t y)
{
	struct dwl_context *context = (struct dwl_context*)data;
	struct wl_surface *s = context->input.touch_surface;
	(void)wl_touch;
	(void)time;
	if (s)
		gvm_push(context, s, DWL_EVENT_TYPE_FINGER_MOTION, id, gvm_to_buffer_x(s, x), gvm_to_buffer_y(s, y));
}

static void touch_frame(void *data, struct wl_touch *wl_touch)
{
	(void)data;
	(void)wl_touch;
}

static void touch_cancel(void *data, struct wl_touch *wl_touch)
{
	struct dwl_context *context = (struct dwl_context*)data;
	(void)wl_touch;
	if (context->input.touch_surface)
		gvm_push(context, context->input.touch_surface, DWL_EVENT_TYPE_FINGER_CANCEL, 0, 0, 0);
}

static void touch_shape(void *data, struct wl_touch *wl_touch, int32_t id, wl_fixed_t major, wl_fixed_t minor)
{
	(void)data;
	(void)wl_touch;
	(void)id;
	(void)major;
	(void)minor;
}

static void touch_orientation(void *data, struct wl_touch *wl_touch, int32_t id, wl_fixed_t orientation)
{
	(void)data;
	(void)wl_touch;
	(void)id;
	(void)orientation;
}

static const struct wl_touch_listener gvm_touch_listener = {
	.down = touch_down,
	.up = touch_up,
	.motion = touch_motion,
	.frame = touch_frame,
	.cancel = touch_cancel,
	.shape = touch_shape,
	.orientation = touch_orientation,
};

static void wl_seat_capabilities(void *data, struct wl_seat *wl_seat,
				 uint32_t capabilities)
{
	struct dwl_context *context = (struct dwl_context*)data;
	struct input *input = &context->input;
	bool have_keyboard = capabilities & WL_SEAT_CAPABILITY_KEYBOARD;
	bool have_pointer = capabilities & WL_SEAT_CAPABILITY_POINTER;

	if (have_keyboard && input->wl_keyboard == NULL) {
		input->wl_keyboard = wl_seat_get_keyboard(wl_seat);
		wl_keyboard_add_listener(input->wl_keyboard, &wl_keyboard_listener, context);
	} else if (!have_keyboard && input->wl_keyboard != NULL) {
		wl_keyboard_release(input->wl_keyboard);
		input->wl_keyboard = NULL;
	}

	bool have_touch = capabilities & WL_SEAT_CAPABILITY_TOUCH;
	if (have_touch && input->wl_touch == NULL) {
		input->wl_touch = wl_seat_get_touch(wl_seat);
		wl_touch_add_listener(input->wl_touch, &gvm_touch_listener, context);
	} else if (!have_touch && input->wl_touch != NULL) {
		wl_touch_release(input->wl_touch);
		input->wl_touch = NULL;
	}

	if (have_pointer && input->wl_pointer == NULL) {
		input->wl_pointer = wl_seat_get_pointer(wl_seat);
		wl_pointer_add_listener(input->wl_pointer, &wl_pointer_listener, context);
	} else if (!have_pointer && input->wl_pointer != NULL) {
		wl_pointer_release(input->wl_pointer);
		input->wl_pointer = NULL;
	}
}

static void wl_seat_name(void *data, struct wl_seat *wl_seat, const char *name)
{
	(void)data;
	(void)wl_seat;
	(void)name;
}

static const struct wl_seat_listener wl_seat_listener = {
	.capabilities = wl_seat_capabilities,
	.name = wl_seat_name,
};

static void dwl_context_output_add(struct dwl_context *context,
				   struct wl_output *wl_output, uint32_t id)
{
	size_t i;
	struct output *output;
	outputs_for_each(context, i, output)
	{
		if (output->output == NULL) {
			context->output_added = true;
			output->id = id;
			output->output = wl_output;
			output->context = context;
			output->current_scale = 1000;
			output->device_scale_factor = 1000;
			// This is a fun little hack from reveman. The idea is
			// that the first display will be internal and never get
			// removed.
			output->internal = i == 0;
			wl_output_add_listener(output->output, &output_listener,
					       output);
			return;
		}
	}
}

static void dwl_context_output_remove_destroy(struct dwl_context *context,
					      uint32_t id)
{
	size_t i;
	struct output *output;
	outputs_for_each(context, i, output)
	{
		if (output->id == id) {
			if (output->aura_output)
				zaura_output_destroy(output->aura_output);
			wl_output_destroy(output->output);
			memset(output, 0, sizeof(struct output));
			return;
		}
	}
}

static void dwl_context_output_get_aura(struct dwl_context *context)
{
	if (!context->ifaces.aura)
		return;

	size_t i;
	struct output *output;
	outputs_for_each(context, i, output)
	{
		if (output->output != NULL && output->aura_output == NULL) {
			output->aura_output = zaura_shell_get_aura_output(
			    context->ifaces.aura, output->output);
			zaura_output_add_listener(
			    output->aura_output, &aura_output_listener, output);
		}
	}
}

static void registry_global(void *data, struct wl_registry *registry,
			    uint32_t id, const char *interface,
			    uint32_t version)
{
	(void)version;
	struct interfaces *ifaces = (struct interfaces *)data;
	if (strcmp(interface, wl_compositor_interface.name) == 0) {
		ifaces->compositor = (struct wl_compositor *)wl_registry_bind(
		    registry, id, &wl_compositor_interface, 3);
	} else if (strcmp(interface, wl_subcompositor_interface.name) == 0) {
		ifaces->subcompositor =
		    (struct wl_subcompositor *)wl_registry_bind(
			registry, id, &wl_subcompositor_interface, 1);
	} else if (strcmp(interface, wl_shm_interface.name) == 0) {
		ifaces->shm = (struct wl_shm *)wl_registry_bind(
		    registry, id, &wl_shm_interface, 1);
	} else if (strcmp(interface, wl_seat_interface.name) == 0) {
		ifaces->seat = (struct wl_seat *)wl_registry_bind(
		    registry, id, &wl_seat_interface, 5);
		wl_seat_add_listener(ifaces->seat, &wl_seat_listener, ifaces->context);
	} else if (strcmp(interface, wl_output_interface.name) == 0) {
		/* iceland gvm: v4 for the output name (CROSVM_GVM_OUTPUT) */
		struct wl_output *output = (struct wl_output *)wl_registry_bind(
		    registry, id, &wl_output_interface, version < 4 ? (version < 2 ? version : 2) : 4);
		dwl_context_output_add(ifaces->context, output, id);
	} else if (strcmp(interface, "zaura_shell") == 0 && version >= 6) {
		ifaces->aura = (struct zaura_shell *)wl_registry_bind(
		    registry, id, &zaura_shell_interface, 6);
	} else if (strcmp(interface, "zwp_linux_dmabuf_v1") == 0) {
		ifaces->linux_dmabuf =
		    (struct zwp_linux_dmabuf_v1 *)wl_registry_bind(
			registry, id, &zwp_linux_dmabuf_v1_interface, 1);
	} else if (strcmp(interface, xdg_wm_base_interface.name) == 0) {
		ifaces->xdg_wm_base = (struct xdg_wm_base *)wl_registry_bind(
		    registry, id, &xdg_wm_base_interface, 1);
		xdg_wm_base_add_listener(ifaces->xdg_wm_base, &xdg_wm_base_listener,
			NULL);
	} else if (strcmp(interface, "wp_viewporter") == 0) {
		ifaces->viewporter = (struct wp_viewporter *)wl_registry_bind(
		    registry, id, &wp_viewporter_interface, 1);
	} else if (strcmp(interface, zxdg_decoration_manager_v1_interface.name) == 0) {
		/* iceland gvm: KWin (Plasma) draws a title bar / close button when asked;
		 * Mutter does not offer this global, so GNOME is unchanged. */
		ifaces->decoration_manager = (struct zxdg_decoration_manager_v1 *)wl_registry_bind(
		    registry, id, &zxdg_decoration_manager_v1_interface, 1);
	} else if (strcmp(interface, "wp_virtio_gpu_metadata_v1") == 0) {
		ifaces->virtio_gpu_metadata =
			(struct wp_virtio_gpu_metadata_v1 *)wl_registry_bind(
			registry, id, &wp_virtio_gpu_metadata_v1_interface, 1);
	}
}

static void global_remove(void *data, struct wl_registry *registry, uint32_t id)
{
	(void)registry;

	struct interfaces *ifaces = (struct interfaces *)data;
	// If the ID matches any output, this will remove it. Otherwise, this is
	// a no-op.
	dwl_context_output_remove_destroy(ifaces->context, id);

	if (ifaces->aura &&
	    wl_proxy_get_id((struct wl_proxy *)ifaces->aura) == id) {
		zaura_shell_destroy(ifaces->aura);
		ifaces->aura = NULL;
	}

	// TODO(zachr): deal with the removal of some of the required
	// interfaces.
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global, .global_remove = global_remove};

static void toplevel_configure(void *data,
			       struct xdg_toplevel *xdg_toplevel,
			       int32_t width, int32_t height,
			       struct wl_array *states)
{
	/* iceland gvm (0.2-6): in fullscreen the compositor asks for the output's size. The guest display keeps
	 * its resolution; the viewport scales the buffer to that size (the app picks the monitor's resolution
	 * for fullscreen VMs, so normally this is the plain HiDPI scale) and input is mapped back per axis.
	 * Leaving fullscreen restores the window's normal size. Applied at the next commit (after the ack). */
	struct dwl_surface *surface = (struct dwl_surface *)data;
	bool fullscreen = false;
	uint32_t *st;
	(void)xdg_toplevel;
	wl_array_for_each(st, states)
	{
		if (*st == XDG_TOPLEVEL_STATE_FULLSCREEN)
			fullscreen = true;
	}
	/* iceland gvm (p1 VIN): every configure, for the fullscreen settle analysis */
	fprintf(stderr, "GVM-CONFIGURE %dx%d fullscreen=%d viewport=%d\n", width, height, fullscreen,
		surface && surface->viewport ? 1 : 0);
	fflush(stderr);
	if (!surface)
		return;
	/* p3: the first configure arrives in the creation roundtrip, before the viewport exists; it already
	 * carries the fullscreen size. Keep it: dwl_context_surface_new applies it to the new viewport, so the
	 * window does not first map at its windowed size (KWin then answered with an 800x500 configure). */
	if (!surface->viewport) {
		if (fullscreen && width > 0 && height > 0) {
			surface->fs_width = width;
			surface->fs_height = height;
		}
		return;
	}
	if (fullscreen && width > 0 && height > 0) {
		if (surface->fs_width != width || surface->fs_height != height) {
			surface->fs_width = width;
			surface->fs_height = height;
			wp_viewport_set_destination(surface->viewport, width, height);
			surface->gvm_commit = true;
			fprintf(stderr, "GVM-FULLSCREEN on %dx%d (buffer %ux%u)\n", width, height, surface->width,
				surface->height);
			fflush(stderr);
		}
	} else if (surface->fs_width > 0) {
		surface->fs_width = 0;
		surface->fs_height = 0;
		wp_viewport_set_destination(surface->viewport, ceil(surface->width / surface->scale),
					    ceil(surface->height / surface->scale));
		surface->gvm_commit = true;
		fprintf(stderr, "GVM-FULLSCREEN off\n");
		fflush(stderr);
	}
}

static void toplevel_close(void *data,
			   struct xdg_toplevel *xdg_toplevel)
{
	(void)xdg_toplevel;
	struct dwl_surface *surface = (struct dwl_surface *)data;
	/* iceland gvm: upstream turns a window close into a hard VM exit (no guest
	 * shutdown). KWin's title-bar close button makes that one tap away, so by
	 * default only report it: the iceland-vms supervisor sees this line and
	 * shuts the guest down cleanly through its agent. CROSVM_GVM_CLOSE=exit
	 * restores the upstream behaviour. */
	const char *mode = getenv("CROSVM_GVM_CLOSE");
	if (mode && strcmp(mode, "exit") == 0) {
		surface->close_requested = true;
		return;
	}
	fprintf(stderr, "GVM-WINDOW-CLOSE requested by the compositor\n");
	fflush(stderr);
}

static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure, .close = toplevel_close};

static void xdg_surface_configure_handler(void *data,
					  struct xdg_surface *xdg_surface,
					  uint32_t serial)
{
	struct dwl_surface *surface = (struct dwl_surface *)data;
	xdg_surface_ack_configure(xdg_surface, serial);
	/* iceland gvm: a viewport change of the toplevel configure needs a commit (an idle guest sends none) */
	if (surface && surface->gvm_commit) {
		surface->gvm_commit = false;
		wl_surface_commit(surface->wl_surface);
	}
}

static const struct xdg_surface_listener xdg_surface_listener = {
	.configure = xdg_surface_configure_handler
};

static void surface_enter(void *data, struct wl_surface *wl_surface,
			  struct wl_output *wl_output)
{
	struct dwl_surface *surface = (struct dwl_surface *)data;

	struct output *output =
	    (struct output *)wl_output_get_user_data(wl_output);

	surface->scale = (output->device_scale_factor / 1000.0) *
			 (output->current_scale / 1000.0);

	if (surface->viewport && surface->fs_width > 0) {
		/* iceland gvm: fullscreen keeps the compositor's size */
		wp_viewport_set_destination(surface->viewport, surface->fs_width, surface->fs_height);
	} else if (surface->viewport) {
		wp_viewport_set_destination(
		    surface->viewport, ceil(surface->width / surface->scale),
		    ceil(surface->height / surface->scale));
	} else {
		wl_surface_set_buffer_scale(wl_surface, surface->scale);
	}

	wl_surface_commit(wl_surface);
}

static void surface_leave(void *data, struct wl_surface *wl_surface,
			  struct wl_output *output)
{
	(void)data;
	(void)wl_surface;
	(void)output;
}

static const struct wl_surface_listener surface_listener = {
    .enter = surface_enter, .leave = surface_leave};

static void error_callback_stub(const char *message) {
	(void)message;
}

struct dwl_context *dwl_context_new(dwl_error_callback_type error_callback)
{
	struct dwl_context *ctx = calloc(1, sizeof(struct dwl_context));
	if (!ctx)
		return NULL;
	ctx->ifaces.context = ctx;
	ctx->error_callback = error_callback ? error_callback : error_callback_stub;
	return ctx;
}

void dwl_context_destroy(struct dwl_context **self)
{
	if ((*self)->display)
		wl_display_disconnect((*self)->display);
	free(*self);
	*self = NULL;
}

bool dwl_context_setup(struct dwl_context *self, const char *socket_path)
{
	struct wl_display *display = wl_display_connect(socket_path);
	if (!display) {
		self->error_callback("failed to connect to display");
		return false;
	}
	self->display = display;
	wl_display_set_user_data(display, self);

	struct wl_registry *registry = wl_display_get_registry(display);
	if (!registry) {
		self->error_callback("failed to get registry");
		goto fail;
	}

	struct interfaces *ifaces = &self->ifaces;
	wl_registry_add_listener(registry, &registry_listener, ifaces);
	wl_display_roundtrip(display);
	dwl_context_output_get_aura(self);

	if (!ifaces->shm) {
		self->error_callback("missing interface shm");
		goto fail;
	}
	if (!ifaces->compositor) {
		self->error_callback("missing interface compositor");
		goto fail;
	}
	if (!ifaces->subcompositor) {
		self->error_callback("missing interface subcompositor");
		goto fail;
	}
	if (!ifaces->seat) {
		self->error_callback("missing interface seat");
		goto fail;
	}
	if (!ifaces->linux_dmabuf) {
		self->error_callback("missing interface linux_dmabuf");
		goto fail;
	}
	if (!ifaces->xdg_wm_base) {
		self->error_callback("missing interface xdg_wm_base");
		goto fail;
	}

	return true;

fail:
	wl_display_disconnect(display);
	self->display = NULL;
	return false;
}

int dwl_context_fd(struct dwl_context *self)
{
	return wl_display_get_fd(self->display);
}

void dwl_context_dispatch(struct dwl_context *self)
{
	wl_display_dispatch(self->display);
	if (self->output_added) {
		self->output_added = false;
		dwl_context_output_get_aura(self);
		wl_display_roundtrip(self->display);
	}
}

static void linux_buffer_created(
    void *data, struct zwp_linux_buffer_params_v1 *zwp_linux_buffer_params_v1,
    struct wl_buffer *buffer)
{
	(void)zwp_linux_buffer_params_v1;
	struct dwl_dmabuf *dmabuf = (struct dwl_dmabuf *)data;
	dmabuf->buffer = buffer;
}

static void linux_buffer_failed(
    void *data, struct zwp_linux_buffer_params_v1 *zwp_linux_buffer_params_v1)
{
	(void)data;
	(void)zwp_linux_buffer_params_v1;
}

static const struct zwp_linux_buffer_params_v1_listener linux_buffer_listener =
    {.created = linux_buffer_created, .failed = linux_buffer_failed};

static void dmabuf_buffer_release(void *data, struct wl_buffer *buffer)
{
	struct dwl_dmabuf *dmabuf = (struct dwl_dmabuf *)data;
	(void)buffer;

	dmabuf->in_use = false;
}

static const struct wl_buffer_listener dmabuf_buffer_listener = {
    .release = dmabuf_buffer_release};

static bool dwl_context_add_dmabuf(struct dwl_context *self,
				   struct dwl_dmabuf *dmabuf)
{
	size_t i;
	for (i = 0; i < MAX_BUFFER_COUNT; i++) {
		if (!self->dmabufs[i]) {
			self->dmabufs[i] = dmabuf;
			return true;
		}
	}

	return false;
}

static void dwl_context_remove_dmabuf(struct dwl_context *self,
				      uint32_t import_id)
{
	size_t i;
	for (i = 0; i < MAX_BUFFER_COUNT; i++) {
		if (self->dmabufs[i] &&
		    self->dmabufs[i]->import_id == import_id) {
			self->dmabufs[i] = NULL;
		}
	}
}

static struct dwl_dmabuf *dwl_context_get_dmabuf(struct dwl_context *self,
					         uint32_t import_id)
{
	size_t i;
	for (i = 0; i < MAX_BUFFER_COUNT; i++) {
		if (self->dmabufs[i] &&
		    self->dmabufs[i]->import_id == import_id) {
			return self->dmabufs[i];
		}
	}

	return NULL;
}

struct dwl_dmabuf *dwl_context_dmabuf_new(struct dwl_context *self,
					  uint32_t import_id,
					  int fd, uint32_t offset,
					  uint32_t stride, uint64_t modifier,
					  uint32_t width, uint32_t height,
					  uint32_t fourcc)
{
	struct dwl_dmabuf *dmabuf = calloc(1, sizeof(struct dwl_dmabuf));
	if (!dmabuf) {
		self->error_callback("failed to allocate dwl_dmabuf");
		return NULL;
	}
	dmabuf->width = width;
	dmabuf->height = height;

	struct zwp_linux_buffer_params_v1 *params =
	    zwp_linux_dmabuf_v1_create_params(self->ifaces.linux_dmabuf);
	if (!params) {
		self->error_callback("failed to allocate zwp_linux_buffer_params_v1");
		free(dmabuf);
		return NULL;
	}

	zwp_linux_buffer_params_v1_add_listener(params, &linux_buffer_listener,
						dmabuf);
	zwp_linux_buffer_params_v1_add(params, fd, 0 /* plane_idx */, offset,
				       stride, modifier >> 32,
				       (uint32_t)modifier);
	zwp_linux_buffer_params_v1_create(params, width, height, fourcc, 0);
	wl_display_roundtrip(self->display);
	zwp_linux_buffer_params_v1_destroy(params);

	if (!dmabuf->buffer) {
		self->error_callback("failed to get wl_buffer for dmabuf");
		free(dmabuf);
		return NULL;
	}

	wl_buffer_add_listener(dmabuf->buffer, &dmabuf_buffer_listener, dmabuf);

	dmabuf->import_id = import_id;
	dmabuf->context = self;
	if (!dwl_context_add_dmabuf(self, dmabuf)) {
		self->error_callback("failed to add dmabuf to context");
		free(dmabuf);
		return NULL;
	}

	return dmabuf;
}

void dwl_dmabuf_destroy(struct dwl_dmabuf **self)
{
	dwl_context_remove_dmabuf((*self)->context, (*self)->import_id);
	wl_buffer_destroy((*self)->buffer);
	free(*self);
	*self = NULL;
}

static void surface_buffer_release(void *data, struct wl_buffer *buffer)
{
	struct dwl_surface *surface = (struct dwl_surface *)data;
	(void)buffer;

	size_t i;
	for (i = 0; i < surface->buffer_count; i++) {
		if (buffer == surface->buffers[i]) {
			surface->buffer_use_bit_mask &= ~(1 << i);
			break;
		}
	}
}

static const struct wl_buffer_listener surface_buffer_listener = {
    .release = surface_buffer_release};

static struct dwl_surface *dwl_context_get_surface(struct dwl_context *self,
					           uint32_t surface_id)
{
	size_t i;
	for (i = 0; i < MAX_BUFFER_COUNT; i++) {
		if (self->surfaces[i] &&
		    self->surfaces[i]->surface_id == surface_id) {
			return self->surfaces[i];
		}
	}

	return NULL;
}

static bool dwl_context_add_surface(struct dwl_context *self,
				    struct dwl_surface *surface)
{
	size_t i;
	for (i = 0; i < MAX_BUFFER_COUNT; i++) {
		if (!self->surfaces[i]) {
			self->surfaces[i] = surface;
			return true;
		}
	}

	return false;
}

static void dwl_context_remove_surface(struct dwl_context *self,
				       uint32_t surface_id)
{
	size_t i;
	for (i = 0; i < MAX_BUFFER_COUNT; i++) {
		if (self->surfaces[i] &&
		    self->surfaces[i]->surface_id == surface_id) {
			self->surfaces[i] = NULL;
		}
	}
}

struct dwl_surface *dwl_context_surface_new(struct dwl_context *self,
					    uint32_t parent_id,
					    uint32_t  surface_id,
					    int shm_fd, size_t shm_size,
					    size_t buffer_size, uint32_t width,
					    uint32_t height, uint32_t stride,
					    uint32_t flags)
{
	if (buffer_size == 0)
		return NULL;

	size_t buffer_count = shm_size / buffer_size;
	if (buffer_count == 0)
		return NULL;
	if (buffer_count > MAX_BUFFER_COUNT)
		return NULL;

	struct dwl_surface *disp_surface =
	    calloc(1, sizeof(struct dwl_surface) +
			  sizeof(struct wl_buffer *) * buffer_count);
	if (!disp_surface)
		return NULL;

	disp_surface->context = self;
	disp_surface->width = width;
	disp_surface->height = height;
	disp_surface->scale = DEFAULT_SCALE;
	disp_surface->buffer_count = buffer_count;

	struct wl_shm_pool *shm_pool =
	    wl_shm_create_pool(self->ifaces.shm, shm_fd, shm_size);
	if (!shm_pool) {
		self->error_callback("failed to make shm pool");
		goto fail;
	}

	size_t i;
	uint32_t format = (flags & DWL_SURFACE_FLAG_HAS_ALPHA)?
		WL_SHM_FORMAT_ARGB8888:WL_SHM_FORMAT_XRGB8888;

	for (i = 0; i < buffer_count; i++) {
		struct wl_buffer *buffer = wl_shm_pool_create_buffer(
		    shm_pool, buffer_size * i, width, height, stride, format);
		if (!buffer) {
			self->error_callback("failed to create buffer");
			goto fail;
		}
		disp_surface->buffers[i] = buffer;
	}

	for (i = 0; i < buffer_count; i++)
		wl_buffer_add_listener(disp_surface->buffers[i],
				       &surface_buffer_listener, disp_surface);

	disp_surface->wl_surface =
	    wl_compositor_create_surface(self->ifaces.compositor);
	if (!disp_surface->wl_surface) {
		self->error_callback("failed to make surface");
		goto fail;
	}

	wl_surface_add_listener(disp_surface->wl_surface, &surface_listener,
				disp_surface);

	struct wl_region *region = wl_compositor_create_region(self->ifaces.compositor);
	if (!region) {
		self->error_callback("failed to make region");
		goto fail;
	}

	bool receive_input = (flags & DWL_SURFACE_FLAG_RECEIVE_INPUT);
	if (receive_input) {
		/* iceland gvm: the region is in surface (logical) coordinates and is clipped to the surface; a
		 * fullscreen viewport can make the surface larger than the buffer, so cover any size */
		wl_region_add(region, 0, 0, 32767, 32767);
	} else {
		// We have to add an empty region because NULL doesn't work
		wl_region_add(region, 0, 0, 0, 0);
	}
	wl_surface_set_input_region(disp_surface->wl_surface, region);
	wl_surface_set_opaque_region(disp_surface->wl_surface, region);
	wl_region_destroy(region);

	if (!parent_id) {
		disp_surface->xdg_surface = xdg_wm_base_get_xdg_surface(
		    self->ifaces.xdg_wm_base, disp_surface->wl_surface);
		if (!disp_surface->xdg_surface) {
			self->error_callback("failed to make xdg shell surface");
			goto fail;
		}

		disp_surface->xdg_toplevel =
		    xdg_surface_get_toplevel(disp_surface->xdg_surface);
		if (!disp_surface->xdg_toplevel) {
			self->error_callback("failed to make toplevel xdg shell surface");
			goto fail;
		}
		/* iceland gvm: window title / app id from the environment ("Unknown" in GNOME otherwise) */
		const char *title = getenv("CROSVM_GVM_WINDOW_TITLE");
		const char *app_id = getenv("CROSVM_GVM_APP_ID");
		xdg_toplevel_set_title(disp_surface->xdg_toplevel, title && *title ? title : "crosvm");
		xdg_toplevel_set_app_id(disp_surface->xdg_toplevel, app_id && *app_id ? app_id : "crosvm");
		xdg_toplevel_add_listener(disp_surface->xdg_toplevel,
					      &toplevel_listener, disp_surface);
		if (self->ifaces.decoration_manager &&
		    !(getenv("CROSVM_GVM_NO_SSD") && *getenv("CROSVM_GVM_NO_SSD"))) {
			disp_surface->decoration =
			    zxdg_decoration_manager_v1_get_toplevel_decoration(
				self->ifaces.decoration_manager, disp_surface->xdg_toplevel);
			if (disp_surface->decoration)
				zxdg_toplevel_decoration_v1_set_mode(
				    disp_surface->decoration,
				    ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE);
		}

		xdg_surface_add_listener(disp_surface->xdg_surface,
					     &xdg_surface_listener,
					     disp_surface);
		/* iceland gvm (0.2-6): CROSVM_GVM_FULLSCREEN=1 opens the window fullscreen, on the output named
		 * or numbered (0-based, binding order) by CROSVM_GVM_OUTPUT; unknown or unset = the compositor's choice */
		const char *fs = getenv("CROSVM_GVM_FULLSCREEN");
		if (fs && *fs && strcmp(fs, "0") != 0) {
			const char *want = getenv("CROSVM_GVM_OUTPUT");
			struct wl_output *target = NULL;
			if (want && *want) {
				char *end = NULL;
				long n = strtol(want, &end, 10);
				size_t oi, k = 0;
				struct output *o;
				outputs_for_each(self, oi, o)
				{
					if (!o->output)
						continue;
					if ((end && *end == 0 && (long)k == n) || strcmp(o->name, want) == 0) {
						target = o->output;
						break;
					}
					k++;
				}
			}
			xdg_toplevel_set_fullscreen(disp_surface->xdg_toplevel, target);
		}
		if (self->ifaces.aura) {
			disp_surface->aura = zaura_shell_get_aura_surface(
			    self->ifaces.aura, disp_surface->wl_surface);
			if (!disp_surface->aura) {
				self->error_callback("failed to make aura surface");
				goto fail;
			}
			zaura_surface_set_frame(
			    disp_surface->aura,
			    ZAURA_SURFACE_FRAME_TYPE_NORMAL);
		}

		// signal that the surface is ready to be configured
		wl_surface_commit(disp_surface->wl_surface);

		// wait for the surface to be configured
		wl_display_roundtrip(self->display);
	} else {
		struct dwl_surface *parent_surface =
			dwl_context_get_surface(self, parent_id);

		if (!parent_surface) {
			self->error_callback("failed to find parent_surface");
			goto fail;
		}

		disp_surface->subsurface = wl_subcompositor_get_subsurface(
		    self->ifaces.subcompositor, disp_surface->wl_surface,
		    parent_surface->wl_surface);
		if (!disp_surface->subsurface) {
			self->error_callback("failed to make subsurface");
			goto fail;
		}
		wl_subsurface_set_desync(disp_surface->subsurface);
	}

	if (self->ifaces.viewporter) {
		disp_surface->viewport = wp_viewporter_get_viewport(
		    self->ifaces.viewporter, disp_surface->wl_surface);
		if (!disp_surface->viewport) {
			self->error_callback("failed to make surface viewport");
			goto fail;
		}
		/* iceland gvm (p3): a fullscreen size from the creation roundtrip applies from the first commit */
		if (disp_surface->fs_width > 0 && disp_surface->fs_height > 0) {
			wp_viewport_set_destination(disp_surface->viewport, disp_surface->fs_width,
						    disp_surface->fs_height);
			fprintf(stderr, "GVM-FULLSCREEN on %dx%d (buffer %ux%u, at creation)\n", disp_surface->fs_width,
				disp_surface->fs_height, disp_surface->width, disp_surface->height);
			fflush(stderr);
		}
	}

	if (self->ifaces.virtio_gpu_metadata) {
		disp_surface->virtio_gpu_surface_metadata =
			wp_virtio_gpu_metadata_v1_get_surface_metadata(
				self->ifaces.virtio_gpu_metadata, disp_surface->wl_surface);
		if (!disp_surface->virtio_gpu_surface_metadata) {
			self->error_callback("failed to make surface virtio surface metadata");
			goto fail;
		}
	}

	wl_surface_attach(disp_surface->wl_surface, disp_surface->buffers[0], 0,
			  0);
	wl_surface_damage(disp_surface->wl_surface, 0, 0, width, height);
	wl_shm_pool_destroy(shm_pool);

	// Needed to get outputs before iterating them.
	wl_display_roundtrip(self->display);

	// Assuming that this surface will enter the internal output initially,
	// trigger a surface enter for that output before doing the first
	// surface commit. THis is to avoid unpleasant artifacts when the
	// surface first appears.
	struct output *output;
	outputs_for_each(self, i, output)
	{
		if (output->internal) {
			surface_enter(disp_surface, disp_surface->wl_surface,
				      output->output);
		}
	}

	wl_surface_commit(disp_surface->wl_surface);
	wl_display_flush(self->display);

	disp_surface->surface_id = surface_id;
	if (!dwl_context_add_surface(self, disp_surface)) {
		self->error_callback("failed to add surface to context");
		goto fail;
	}

	return disp_surface;
fail:
	if (disp_surface->virtio_gpu_surface_metadata)
		wp_virtio_gpu_surface_metadata_v1_destroy(
			disp_surface->virtio_gpu_surface_metadata);
	if (disp_surface->viewport)
		wp_viewport_destroy(disp_surface->viewport);
	if (disp_surface->subsurface)
		wl_subsurface_destroy(disp_surface->subsurface);
	if (disp_surface->decoration)
		zxdg_toplevel_decoration_v1_destroy(disp_surface->decoration);
	if (disp_surface->xdg_toplevel)
		xdg_toplevel_destroy(disp_surface->xdg_toplevel);
	if (disp_surface->xdg_surface)
		xdg_surface_destroy(disp_surface->xdg_surface);
	if (disp_surface->aura)
		zaura_surface_destroy(disp_surface->aura);
	if (disp_surface->wl_surface)
		wl_surface_destroy(disp_surface->wl_surface);
	for (i = 0; i < buffer_count; i++)
		if (disp_surface->buffers[i])
			wl_buffer_destroy(disp_surface->buffers[i]);
	if (shm_pool)
		wl_shm_pool_destroy(shm_pool);
	free(disp_surface);
	return NULL;
}

void dwl_surface_destroy(struct dwl_surface **self)
{
	size_t i;

	dwl_context_remove_surface((*self)->context, (*self)->surface_id);
	if ((*self)->virtio_gpu_surface_metadata)
		wp_virtio_gpu_surface_metadata_v1_destroy(
			(*self)->virtio_gpu_surface_metadata);
	if ((*self)->viewport)
		wp_viewport_destroy((*self)->viewport);
	if ((*self)->subsurface)
		wl_subsurface_destroy((*self)->subsurface);
	if ((*self)->decoration)
		zxdg_toplevel_decoration_v1_destroy((*self)->decoration);
	if ((*self)->xdg_toplevel)
		xdg_toplevel_destroy((*self)->xdg_toplevel);
	if ((*self)->xdg_surface)
		xdg_surface_destroy((*self)->xdg_surface);
	if ((*self)->aura)
		zaura_surface_destroy((*self)->aura);
	if ((*self)->wl_surface)
		wl_surface_destroy((*self)->wl_surface);
	for (i = 0; i < (*self)->buffer_count; i++)
		wl_buffer_destroy((*self)->buffers[i]);
	wl_display_flush((*self)->context->display);
	free(*self);
	*self = NULL;
}

void dwl_surface_commit(struct dwl_surface *self)
{
	// It is possible that we are committing frames faster than the
	// compositor can put them on the screen. This may result in dropped
	// frames, but this is acceptable considering there is no good way to
	// apply back pressure to the guest gpu driver right now. The intention
	// of this module is to help bootstrap gpu support, so it does not have
	// to have artifact free rendering.
	wl_surface_commit(self->wl_surface);
	wl_display_flush(self->context->display);
}

bool dwl_surface_buffer_in_use(struct dwl_surface *self, size_t buffer_index)
{
	return (self->buffer_use_bit_mask & (1 << buffer_index)) != 0;
}

void dwl_surface_flip(struct dwl_surface *self, size_t buffer_index)
{
	if (buffer_index >= self->buffer_count)
		return;
	wl_surface_attach(self->wl_surface, self->buffers[buffer_index], 0, 0);
	wl_surface_damage(self->wl_surface, 0, 0, self->width, self->height);
	dwl_surface_commit(self);
	self->buffer_use_bit_mask |= 1 << buffer_index;
}

void dwl_surface_flip_to(struct dwl_surface *self, uint32_t import_id)
{
	// Surface and dmabuf have to exist in same context.
	struct dwl_dmabuf *dmabuf = dwl_context_get_dmabuf(self->context,
							   import_id);
	if (!dmabuf)
		return;

	if (self->width != dmabuf->width || self->height != dmabuf->height)
		return;
	wl_surface_attach(self->wl_surface, dmabuf->buffer, 0, 0);
	wl_surface_damage(self->wl_surface, 0, 0, self->width, self->height);
	dwl_surface_commit(self);
	dmabuf->in_use = true;
}

bool dwl_surface_close_requested(const struct dwl_surface *self)
{
	return self->close_requested;
}

void dwl_surface_set_position(struct dwl_surface *self, uint32_t x, uint32_t y)
{
	if (self->subsurface) {
		wl_subsurface_set_position(self->subsurface, x / self->scale,
					   y / self->scale);
		wl_surface_commit(self->wl_surface);
		wl_display_flush(self->context->display);
	}
}

const void* dwl_surface_descriptor(const struct dwl_surface *self)
{
	return self->wl_surface;
}

bool dwl_context_pending_events(const struct dwl_context *self)
{
	if (self->event_write_pos == self->event_read_pos)
		return false;

	return true;
}

void dwl_context_next_event(struct dwl_context *self, struct dwl_event *event)
{
	memcpy(event, self->event_cbuf + self->event_read_pos,
	       sizeof(struct dwl_event));

	if (++self->event_read_pos == EVENT_BUF_SIZE)
		self->event_read_pos = 0;
}

void dwl_surface_set_scanout_id(struct dwl_surface *self, uint32_t scanout_id)
{
	if (self->virtio_gpu_surface_metadata) {
		wp_virtio_gpu_surface_metadata_v1_set_scanout_id(
			self->virtio_gpu_surface_metadata, scanout_id);
	}
}
