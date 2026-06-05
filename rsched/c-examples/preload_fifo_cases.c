#if defined(ROLE_FIFO_SERVER)

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

#elif defined(ROLE_FIFO_CLIENT)

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <unistd.h>

struct thread_arg {
    const char *path;
    char msg;
};

static void *send_msg(void *raw) {
    struct thread_arg *arg = (struct thread_arg *)raw;
    for (int attempt = 0; attempt < 16; attempt++) {
        sched_yield();
        int fd = open(arg->path, O_WRONLY | O_NONBLOCK);
        if (fd >= 0) {
            if (write(fd, &arg->msg, 1) == 1) {
                close(fd);
                return 0;
            }
            close(fd);
        } else if (errno != ENOENT && errno != ENXIO) {
            return 0;
        }
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s FIFO\n", argv[0]);
        return 2;
    }

    signal(SIGPIPE, SIG_IGN);
    pthread_t a;
    pthread_t b;
    struct thread_arg arg_a = {argv[1], 'A'};
    struct thread_arg arg_b = {argv[1], 'B'};
    if (pthread_create(&a, 0, send_msg, &arg_a) != 0)
        return 2;
    if (pthread_create(&b, 0, send_msg, &arg_b) != 0)
        return 2;
    if (pthread_join(a, 0) != 0)
        return 2;
    if (pthread_join(b, 0) != 0)
        return 2;
    return 0;
}

#else
#error "define one FIFO role"
#endif
