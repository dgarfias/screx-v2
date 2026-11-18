// SPDX-License-Identifier: GPL-2.0
/*
 * screx_snd: sound cards for the iPad's speakers and microphone.
 *
 * The Screx daemon creates a card through /dev/screx_audio when the iPad turns
 * its speakers or microphone on, and the card goes away when the daemon closes
 * the file, like a USB device being plugged in and out. A card has no clock of
 * its own: its position advances only as the daemon reads speaker audio or
 * writes microphone audio, at the pace the iPad plays or records it.
 */
#include <linux/fs.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <sound/core.h>
#include <sound/initval.h>
#include <sound/pcm.h>

#include "screx_snd_uapi.h"

#define SCREX_RATE 48000
#define SCREX_MAX_CHANNELS 2
/* 500 ms of stereo audio. */
#define SCREX_BUFFER_BYTES_MAX (SCREX_RATE * SCREX_MAX_CHANNELS * 2 / 2)
/* Largest single read() or write(). */
#define SCREX_IO_BYTES_MAX (4096 * SCREX_MAX_CHANNELS * 2)

/* Lives as long as the card (card->private_data), which may outlast the daemon's file. */
struct screx_snd {
	struct snd_pcm_substream *substream;
	int direction;
	unsigned int channels;
	/* Protected by the stream lock. Frames consumed or supplied since prepare, wrapped at
	 * the runtime boundary, and frames into the current period. */
	snd_pcm_uframes_t pos;
	snd_pcm_uframes_t period_pos;
	/* Frames of delay after the buffer, reported through runtime->delay. */
	snd_pcm_uframes_t extra_delay;
	u8 *bounce;
};

/* One per open of /dev/screx_audio. */
struct screx_file {
	struct mutex lock;
	struct snd_card *card;
};

/* The cards' parent device. */
static struct miscdevice screx_misc;

static unsigned int screx_frame_bytes(const struct screx_snd *chip)
{
	return chip->channels * 2;
}

static int screx_pcm_open(struct snd_pcm_substream *substream)
{
	struct screx_snd *chip = snd_pcm_substream_chip(substream);
	struct snd_pcm_runtime *runtime = substream->runtime;

	runtime->hw = (struct snd_pcm_hardware) {
		.info = SNDRV_PCM_INFO_MMAP | SNDRV_PCM_INFO_MMAP_VALID |
			SNDRV_PCM_INFO_INTERLEAVED | SNDRV_PCM_INFO_BLOCK_TRANSFER |
			/* The position moves in steps as the daemon reads or writes. */
			SNDRV_PCM_INFO_BATCH,
		.formats = SNDRV_PCM_FMTBIT_S16_LE,
		.rates = SNDRV_PCM_RATE_48000,
		.rate_min = SCREX_RATE,
		.rate_max = SCREX_RATE,
		.channels_min = chip->channels,
		.channels_max = chip->channels,
		.buffer_bytes_max = SCREX_BUFFER_BYTES_MAX,
		.period_bytes_min = 64 * screx_frame_bytes(chip),
		.period_bytes_max = SCREX_BUFFER_BYTES_MAX / 2,
		.periods_min = 2,
		.periods_max = 1024,
	};
	/* Whole periods keep the runtime boundary a multiple of the period size. */
	return snd_pcm_hw_constraint_integer(runtime, SNDRV_PCM_HW_PARAM_PERIODS);
}

static int screx_pcm_close(struct snd_pcm_substream *substream)
{
	return 0;
}

static int screx_pcm_prepare(struct snd_pcm_substream *substream)
{
	struct screx_snd *chip = snd_pcm_substream_chip(substream);

	/* Not running, so read() and write() leave these alone. */
	chip->pos = 0;
	chip->period_pos = 0;
	return 0;
}

static int screx_pcm_trigger(struct snd_pcm_substream *substream, int cmd)
{
	switch (cmd) {
	case SNDRV_PCM_TRIGGER_START:
	case SNDRV_PCM_TRIGGER_STOP:
		/* read() and write() check the stream state; nothing to start or stop. */
		return 0;
	default:
		return -EINVAL;
	}
}

static snd_pcm_uframes_t screx_pcm_pointer(struct snd_pcm_substream *substream)
{
	struct screx_snd *chip = snd_pcm_substream_chip(substream);
	struct snd_pcm_runtime *runtime = substream->runtime;

	runtime->delay = READ_ONCE(chip->extra_delay);
	return chip->pos % runtime->buffer_size;
}

static const struct snd_pcm_ops screx_pcm_ops = {
	.open = screx_pcm_open,
	.close = screx_pcm_close,
	.prepare = screx_pcm_prepare,
	.trigger = screx_pcm_trigger,
	.pointer = screx_pcm_pointer,
};

/*
 * Copies `frames` between chip->bounce and the ring buffer at the current position,
 * wrapping at the end. Speakers copy at most what the application has queued
 * (`frames` is then the queued count).
 */
static void screx_copy(struct screx_snd *chip, snd_pcm_uframes_t frames)
{
	struct snd_pcm_runtime *runtime = chip->substream->runtime;
	snd_pcm_uframes_t offset = chip->pos % runtime->buffer_size;
	snd_pcm_uframes_t first = min(frames, runtime->buffer_size - offset);
	size_t first_bytes = frames_to_bytes(runtime, first);
	size_t rest_bytes = frames_to_bytes(runtime, frames - first);
	u8 *ring = runtime->dma_area + frames_to_bytes(runtime, offset);

	if (chip->direction == SCREX_SND_PLAYBACK) {
		memcpy(chip->bounce, ring, first_bytes);
		memcpy(chip->bounce + first_bytes, runtime->dma_area, rest_bytes);
	} else {
		memcpy(ring, chip->bounce, first_bytes);
		memcpy(runtime->dma_area, chip->bounce + first_bytes, rest_bytes);
	}
}

/*
 * Moves `frames` of audio between chip->bounce and the stream and advances the
 * position by `frames`, whether or not the application kept up, as real hardware
 * would. Called with the stream lock held.
 */
static void screx_transfer(struct screx_snd *chip, snd_pcm_uframes_t frames)
{
	struct snd_pcm_substream *substream = chip->substream;
	struct snd_pcm_runtime *runtime = substream->runtime;

	if (chip->direction == SCREX_SND_PLAYBACK) {
		/* Our own position, not status->hw_ptr, which only updates on period boundaries. */
		snd_pcm_sframes_t queued = READ_ONCE(runtime->control->appl_ptr) - chip->pos;

		if (queued < 0)
			queued += runtime->boundary;
		if (queued > runtime->buffer_size)
			queued = 0;
		/* The rest of the bounce buffer stays silent. */
		screx_copy(chip, min_t(snd_pcm_uframes_t, frames, queued));
	} else {
		screx_copy(chip, frames);
	}

	chip->pos += frames;
	if (chip->pos >= runtime->boundary)
		chip->pos -= runtime->boundary;
	chip->period_pos += frames;
	if (chip->period_pos >= runtime->period_size) {
		chip->period_pos %= runtime->period_size;
		snd_pcm_period_elapsed_under_stream_lock(substream);
	}
}

/* Runs one transfer of `count` bytes already in (or bound for) chip->bounce. */
static void screx_run(struct screx_snd *chip, size_t count)
{
	struct snd_pcm_substream *substream = chip->substream;
	unsigned long flags;

	snd_pcm_stream_lock_irqsave(substream, flags);
	/* Not running: speakers return silence, microphone audio is dropped. */
	if (substream->runtime && snd_pcm_running(substream))
		screx_transfer(chip, count / screx_frame_bytes(chip));
	snd_pcm_stream_unlock_irqrestore(substream, flags);
}

/* Returns the file's card of the given direction, or an error. Called with sf->lock held. */
static struct screx_snd *screx_chip(struct screx_file *sf, int direction)
{
	struct screx_snd *chip;

	if (!sf->card)
		return ERR_PTR(-ENODEV);
	chip = sf->card->private_data;
	if (chip->direction != direction)
		return ERR_PTR(-EINVAL);
	return chip;
}

static ssize_t screx_read(struct file *file, char __user *buf, size_t count, loff_t *ppos)
{
	struct screx_file *sf = file->private_data;
	struct screx_snd *chip;
	ssize_t ret;

	mutex_lock(&sf->lock);
	chip = screx_chip(sf, SCREX_SND_PLAYBACK);
	if (IS_ERR(chip)) {
		ret = PTR_ERR(chip);
		goto out;
	}
	if (count % screx_frame_bytes(chip)) {
		ret = -EINVAL;
		goto out;
	}
	count = min_t(size_t, count, SCREX_IO_BYTES_MAX);

	memset(chip->bounce, 0, count);
	screx_run(chip, count);
	ret = copy_to_user(buf, chip->bounce, count) ? -EFAULT : count;
out:
	mutex_unlock(&sf->lock);
	return ret;
}

static ssize_t screx_write(struct file *file, const char __user *buf, size_t count,
			   loff_t *ppos)
{
	struct screx_file *sf = file->private_data;
	struct screx_snd *chip;
	ssize_t ret;

	mutex_lock(&sf->lock);
	chip = screx_chip(sf, SCREX_SND_CAPTURE);
	if (IS_ERR(chip)) {
		ret = PTR_ERR(chip);
		goto out;
	}
	if (count % screx_frame_bytes(chip)) {
		ret = -EINVAL;
		goto out;
	}
	count = min_t(size_t, count, SCREX_IO_BYTES_MAX);

	if (copy_from_user(chip->bounce, buf, count)) {
		ret = -EFAULT;
		goto out;
	}
	screx_run(chip, count);
	ret = count;
out:
	mutex_unlock(&sf->lock);
	return ret;
}

static void screx_card_private_free(struct snd_card *card)
{
	struct screx_snd *chip = card->private_data;

	kfree(chip->bounce);
}

static int screx_create_card(struct screx_file *sf, const struct screx_snd_create *req)
{
	bool playback = req->direction == SCREX_SND_PLAYBACK;
	const char *name = req->name[0] ? req->name : "Screx";
	struct snd_card *card;
	struct screx_snd *chip;
	struct snd_pcm *pcm;
	int err;

	if (req->rate != SCREX_RATE || req->channels < 1 || req->channels > SCREX_MAX_CHANNELS ||
	    (req->direction != SCREX_SND_PLAYBACK && req->direction != SCREX_SND_CAPTURE))
		return -EINVAL;

	err = snd_card_new(screx_misc.this_device, SNDRV_DEFAULT_IDX1,
			   playback ? "ScrexSpk" : "ScrexMic", THIS_MODULE, sizeof(*chip), &card);
	if (err < 0)
		return err;
	chip = card->private_data;
	chip->direction = req->direction;
	chip->channels = req->channels;
	chip->bounce = kzalloc(SCREX_IO_BYTES_MAX, GFP_KERNEL);
	if (!chip->bounce) {
		snd_card_free(card);
		return -ENOMEM;
	}
	card->private_free = screx_card_private_free;

	strscpy(card->driver, "screx_snd", sizeof(card->driver));
	strscpy(card->shortname, name, sizeof(card->shortname));
	snprintf(card->longname, sizeof(card->longname), "%s (Screx)", name);

	err = snd_pcm_new(card, name, 0, playback, !playback, &pcm);
	if (err < 0)
		goto fail;
	pcm->private_data = chip;
	strscpy(pcm->name, name, sizeof(pcm->name));
	snd_pcm_set_ops(pcm, req->direction, &screx_pcm_ops);
	snd_pcm_set_managed_buffer_all(pcm, SNDRV_DMA_TYPE_VMALLOC, NULL, 0,
				       SCREX_BUFFER_BYTES_MAX);
	chip->substream = pcm->streams[req->direction].substream;

	err = snd_card_register(card);
	if (err < 0)
		goto fail;
	sf->card = card;
	return 0;

fail:
	snd_card_free(card);
	return err;
}

static long screx_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	struct screx_file *sf = file->private_data;
	struct screx_snd_create req;
	u32 delay;
	long ret;

	mutex_lock(&sf->lock);
	switch (cmd) {
	case SCREX_SND_IOC_CREATE:
		if (sf->card) {
			ret = -EBUSY;
			break;
		}
		if (copy_from_user(&req, (void __user *)arg, sizeof(req))) {
			ret = -EFAULT;
			break;
		}
		req.name[sizeof(req.name) - 1] = '\0';
		ret = screx_create_card(sf, &req);
		break;
	case SCREX_SND_IOC_SET_DELAY:
		if (!sf->card) {
			ret = -ENODEV;
			break;
		}
		if (get_user(delay, (u32 __user *)arg)) {
			ret = -EFAULT;
			break;
		}
		/* Ten seconds is far beyond anything real. */
		if (delay > 10 * SCREX_RATE) {
			ret = -EINVAL;
			break;
		}
		WRITE_ONCE(((struct screx_snd *)sf->card->private_data)->extra_delay, delay);
		ret = 0;
		break;
	default:
		ret = -ENOTTY;
	}
	mutex_unlock(&sf->lock);
	return ret;
}

static int screx_open(struct inode *inode, struct file *file)
{
	struct screx_file *sf = kzalloc(sizeof(*sf), GFP_KERNEL);

	if (!sf)
		return -ENOMEM;
	mutex_init(&sf->lock);
	file->private_data = sf;
	return nonseekable_open(inode, file);
}

static int screx_release(struct inode *inode, struct file *file)
{
	struct screx_file *sf = file->private_data;

	if (sf->card) {
		/* Unplug now; the card is freed once applications close it. */
		snd_card_disconnect(sf->card);
		snd_card_free_when_closed(sf->card);
	}
	mutex_destroy(&sf->lock);
	kfree(sf);
	return 0;
}

static const struct file_operations screx_fops = {
	.owner = THIS_MODULE,
	.open = screx_open,
	.release = screx_release,
	.read = screx_read,
	.write = screx_write,
	.unlocked_ioctl = screx_ioctl,
	.compat_ioctl = compat_ptr_ioctl,
};

static struct miscdevice screx_misc = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "screx_audio",
	.fops = &screx_fops,
	.mode = 0600,
};

module_misc_device(screx_misc);

MODULE_DESCRIPTION("Screx iPad speakers and microphone");
MODULE_LICENSE("GPL");
