#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s FIFO\n", argv[0]);
        return 2;
    }

    unlink(argv[1]);
    if (mkfifo(argv[1], 0600) != 0) {
        perror("mkfifo");
        return 2;
    }

    int fd = open(argv[1], O_RDONLY | O_NONBLOCK);
    if (fd < 0) {
        perror("open fifo");
        return 2;
    }
    sched_yield();

    for (int i = 0; i < 8; i++)
        sched_yield();

    char c = 0;
    ssize_t n = read(fd, &c, 1);
    if (n == 1 && (c == 'A' || c == 'B')) {
        printf("seen=%c\n", c);
    } else if (n == 0 || (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))) {
        printf("seen=NONE\n");
    } else {
        perror("read");
        return 2;
    }
    close(fd);
    unlink(argv[1]);
    return 0;
}
