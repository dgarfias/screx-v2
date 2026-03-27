#include <fcntl.h>
#include <string.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <linux/videodev2.h>

/* Opens a v4l2loopback device as VIDEO_OUTPUT and sets MJPEG format.
   Returns the open fd on success, or a negative error code. */
int screx_v4l2_open_output(const char *device, int width, int height, int fps) {
    int fd = open(device, O_RDWR);
    if (fd < 0)
        return -1;

    struct v4l2_format fmt;
    memset(&fmt, 0, sizeof(fmt));
    fmt.type = V4L2_BUF_TYPE_VIDEO_OUTPUT;
    fmt.fmt.pix.width = width;
    fmt.fmt.pix.height = height;
    fmt.fmt.pix.pixelformat = V4L2_PIX_FMT_MJPEG;
    fmt.fmt.pix.field = V4L2_FIELD_NONE;
    fmt.fmt.pix.sizeimage = width * height * 2;

    if (ioctl(fd, VIDIOC_S_FMT, &fmt) < 0) {
        close(fd);
        return -2;
    }

    if (fps > 0) {
        struct v4l2_streamparm parm;
        memset(&parm, 0, sizeof(parm));
        parm.type = V4L2_BUF_TYPE_VIDEO_OUTPUT;
        parm.parm.output.timeperframe.numerator = 1;
        parm.parm.output.timeperframe.denominator = fps;

        if (ioctl(fd, VIDIOC_S_PARM, &parm) < 0) {
            close(fd);
            return -3;
        }
    }

    return fd;
}
