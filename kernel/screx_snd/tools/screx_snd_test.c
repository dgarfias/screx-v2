// SPDX-License-Identifier: GPL-2.0
/*
 * Stands in for the daemon: creates the card, then reads 10 ms of audio every 10 ms
 * and writes it to stdout as raw s16le 48 kHz stereo. Prints the peak level each
 * second to stderr.
 *
 *   sudo ./screx_snd_test "iPad Test" 20 [delay_ms] > out.raw
 */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include "../screx_snd_uapi.h"

#define CHUNK_FRAMES 480
#define CHUNK_BYTES (CHUNK_FRAMES * 4)

int main(int argc, char **argv)
{
	struct screx_snd_create req = {
		.rate = 48000, .channels = 2, .direction = SCREX_SND_PLAYBACK,
	};
	int16_t buf[CHUNK_FRAMES * 2];
	struct timespec next;
	int seconds, fd, peak = 0;
	long chunk;

	if (argc < 3) {
		fprintf(stderr, "usage: %s NAME SECONDS [DELAY_MS] > out.raw\n", argv[0]);
		return 2;
	}
	seconds = atoi(argv[2]);
	strncpy(req.name, argv[1], sizeof(req.name) - 1);

	fd = open("/dev/screx_audio", O_RDWR);
	if (fd < 0) {
		perror("open /dev/screx_audio");
		return 1;
	}
	if (ioctl(fd, SCREX_SND_IOC_CREATE, &req) < 0) {
		perror("SCREX_SND_IOC_CREATE");
		return 1;
	}
	if (argc > 3) {
		uint32_t delay = (uint32_t)atoi(argv[3]) * 48;

		if (ioctl(fd, SCREX_SND_IOC_SET_DELAY, &delay) < 0) {
			perror("SCREX_SND_IOC_SET_DELAY");
			return 1;
		}
	}
	fprintf(stderr, "card created; reading for %d s\n", seconds);

	clock_gettime(CLOCK_MONOTONIC, &next);
	for (chunk = 0; chunk < seconds * 100L; chunk++) {
		next.tv_nsec += 10 * 1000 * 1000;
		if (next.tv_nsec >= 1000000000L) {
			next.tv_nsec -= 1000000000L;
			next.tv_sec++;
		}
		clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &next, NULL);

		if (read(fd, buf, CHUNK_BYTES) != CHUNK_BYTES) {
			perror("read");
			return 1;
		}
		for (int i = 0; i < CHUNK_FRAMES * 2; i++) {
			int v = abs(buf[i]);

			if (v > peak)
				peak = v;
		}
		if (fwrite(buf, 1, CHUNK_BYTES, stdout) != CHUNK_BYTES)
			return 1;
		if (chunk % 100 == 99) {
			fprintf(stderr, "t=%lds peak=%d\n", (chunk + 1) / 100, peak);
			peak = 0;
		}
	}
	close(fd);
	return 0;
}
