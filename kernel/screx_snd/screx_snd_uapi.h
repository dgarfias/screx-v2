/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
/*
 * Interface between the screx_snd kernel module and the Screx daemon.
 *
 * The daemon opens /dev/screx_audio and issues SCREX_SND_IOC_CREATE to add a
 * sound card with one PCM: speakers (playback) or a microphone (capture).
 * Audio is s16le, interleaved. For speakers, each read() consumes that many
 * bytes of what applications played; for a microphone, each write() supplies
 * audio for applications to record. Either way the card's position advances
 * only as the daemon reads or writes; the card has no clock of its own.
 * Closing the file removes the card.
 */
#ifndef _UAPI_SCREX_SND_H
#define _UAPI_SCREX_SND_H

#include <linux/ioctl.h>
#include <linux/types.h>

#define SCREX_SND_NAME_LEN 80

#define SCREX_SND_PLAYBACK 0
#define SCREX_SND_CAPTURE 1

struct screx_snd_create {
	/* Card name shown to users, NUL-terminated. */
	char name[SCREX_SND_NAME_LEN];
	/* Only 48000 Hz is supported. */
	__u32 rate;
	/* 1 or 2. */
	__u32 channels;
	/* SCREX_SND_PLAYBACK or SCREX_SND_CAPTURE. */
	__u32 direction;
};

#define SCREX_SND_IOC_CREATE _IOW('X', 0x01, struct screx_snd_create)
/* Delay after the card's buffer (network + iPad playback), in frames. */
#define SCREX_SND_IOC_SET_DELAY _IOW('X', 0x02, __u32)

#endif /* _UAPI_SCREX_SND_H */
