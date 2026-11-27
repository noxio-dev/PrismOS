/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
/*
 * drmtest -- milestone M3 of docs/XORG.md: libdrm talks to /dev/dri/card0
 * and drmModeGetResources() returns the virtual connector.
 *
 *   drmtest [/dev/dri/card0]
 *
 * Exit status: 0 = a connected connector was found, 1 = resources came
 * back but nothing is connected, 2 = the node did not open, 3 = libdrm
 * could not read the mode resources.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <xf86drm.h>
#include <xf86drmMode.h>

static const char *connection_name(drmModeConnection c)
{
	switch (c) {
	case DRM_MODE_CONNECTED:
		return "connected";
	case DRM_MODE_DISCONNECTED:
		return "disconnected";
	default:
		return "unknown";
	}
}

int main(int argc, char **argv)
{
	const char *path = argc > 1 ? argv[1] : "/dev/dri/card0";
	int fd = open(path, O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		printf("drmtest: open %s: %s\n", path, strerror(errno));
		return 2;
	}

	drmVersionPtr v = drmGetVersion(fd);
	if (v) {
		printf("drmtest: driver %s %d.%d.%d (%s)\n", v->name,
		       v->version_major, v->version_minor,
		       v->version_patchlevel, v->desc);
		drmFreeVersion(v);
	} else {
		printf("drmtest: drmGetVersion failed\n");
	}

	/* the sysfs half of libdrm: /sys/dev/char/226:0/device/... */
	printf("drmtest: node type %d (0 = primary), KMS %s\n",
	       drmGetNodeTypeFromFd(fd), drmIsKMS(fd) ? "yes" : "no");
	char *name = drmGetDeviceNameFromFd2(fd);
	printf("drmtest: device name %s\n", name ? name : "(none)");
	free(name);

	drmModeResPtr r = drmModeGetResources(fd);
	if (!r) {
		printf("drmtest: drmModeGetResources failed: %s\n",
		       strerror(errno));
		close(fd);
		return 3;
	}
	printf("drmtest: %d crtcs, %d connectors, %d encoders, %d fbs, "
	       "size %ux%u..%ux%u\n",
	       r->count_crtcs, r->count_connectors, r->count_encoders,
	       r->count_fbs, r->min_width, r->min_height, r->max_width,
	       r->max_height);

	int connected = 0;
	for (int i = 0; i < r->count_connectors; i++) {
		drmModeConnectorPtr c = drmModeGetConnector(fd, r->connectors[i]);
		if (!c)
			continue;
		printf("drmtest: connector %u: %s-%u %s, %d modes",
		       c->connector_id,
		       drmModeGetConnectorTypeName(c->connector_type),
		       c->connector_type_id, connection_name(c->connection),
		       c->count_modes);
		if (c->count_modes > 0)
			printf(", first %s %ux%u@%u", c->modes[0].name,
			       c->modes[0].hdisplay, c->modes[0].vdisplay,
			       c->modes[0].vrefresh);
		printf("\n");
		if (c->connection == DRM_MODE_CONNECTED)
			connected++;
		drmModeFreeConnector(c);
	}
	drmModeFreeResources(r);
	close(fd);

	printf("drmtest: %s\n", connected ? "PASS" : "FAIL (nothing connected)");
	return connected ? 0 : 1;
}
