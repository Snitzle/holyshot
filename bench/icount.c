// icount CMD...: run CMD, print instructions retired by it and all its threads/children (user+kernel if allowed)
#include <linux/perf_event.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <stdlib.h>
static int open_ev(pid_t pid, uint64_t cfg, int kernel) {
    struct perf_event_attr a; memset(&a, 0, sizeof a);
    a.size = sizeof a; a.type = PERF_TYPE_HARDWARE; a.config = cfg;
    a.disabled = 1; a.inherit = 1; a.enable_on_exec = 1; a.exclude_kernel = !kernel; a.exclude_hv = 1;
    return syscall(SYS_perf_event_open, &a, pid, -1, -1, 0);
}
int main(int argc, char **argv) {
    int go[2]; pipe(go);
    pid_t pid = fork();
    if (pid == 0) { char c; close(go[1]); read(go[0], &c, 1); execvp(argv[1], argv + 1); _exit(127); }
    int kernel = 1;
    int fd = open_ev(pid, PERF_COUNT_HW_INSTRUCTIONS, 1);
    if (fd < 0) { kernel = 0; fd = open_ev(pid, PERF_COUNT_HW_INSTRUCTIONS, 0); }
    if (fd < 0) { perror("perf_event_open"); }
    close(go[1]);
    int st; waitpid(pid, &st, 0);
    uint64_t n = 0; if (fd >= 0) read(fd, &n, sizeof n);
    fprintf(stderr, "ICOUNT %llu %s\n", (unsigned long long)n, kernel ? "user+kernel" : "user");
    return WIFEXITED(st) ? WEXITSTATUS(st) : 1;
}
