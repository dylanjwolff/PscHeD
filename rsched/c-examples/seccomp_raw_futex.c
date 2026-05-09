#include "rsched.h"

#include <linux/futex.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    int word = 0;

    rsched_init();
    syscall(SYS_futex, &word, FUTEX_WAKE, 1, 0, 0, 0);
    return 0;
}
