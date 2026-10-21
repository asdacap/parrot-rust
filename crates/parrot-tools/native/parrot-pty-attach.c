#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

static const char *program_name = "parrot-pty-attach";

struct child_error
{
    int error;
    char operation[24];
};

static int report_error_code(const char *operation, int error)
{
    (void)fprintf(stderr, "%s: %s: %s\n", program_name, operation, strerror(error));
    return EXIT_FAILURE;
}

static int report_error(const char *operation)
{
    return report_error_code(operation, errno);
}

static int report_usage(void)
{
    (void)fprintf(
        stderr,
        "usage: %s --bridge SLAVE -- BWRAP [ARGS...]\n"
        "       %s --attach -- COMMAND [ARGS...]\n",
        program_name,
        program_name);
    return EXIT_FAILURE;
}

static bool is_separator(const char *argument)
{
    return strcmp(argument, "--") == 0;
}

static int write_all(int descriptor, const void *buffer, size_t length)
{
    const unsigned char *cursor = buffer;
    while (length > 0)
    {
        ssize_t written = write(descriptor, cursor, length);
        if (written == -1)
        {
            if (errno == EINTR)
            {
                continue;
            }

            return -1;
        }

        cursor += (size_t)written;
        length -= (size_t)written;
    }

    return 0;
}

static void report_child_error(int descriptor, const char *operation)
{
    struct child_error failure;
    failure.error = errno;

    size_t index = 0;
    while (index + 1 < sizeof(failure.operation) && operation[index] != '\0')
    {
        failure.operation[index] = operation[index];
        ++index;
    }

    while (index < sizeof(failure.operation))
    {
        failure.operation[index] = '\0';
        ++index;
    }

    (void)write_all(descriptor, &failure, sizeof(failure));
}

static int validate_tty(int descriptor, dev_t *device)
{
    struct stat status;
    if (fstat(descriptor, &status) == -1)
    {
        return report_error("fstat");
    }

    if (!S_ISCHR(status.st_mode) || isatty(descriptor) != 1)
    {
        return report_error_code("validate tty", ENOTTY);
    }

    if (device != NULL)
    {
        *device = status.st_rdev;
    }

    return EXIT_SUCCESS;
}

static void run_bridge_child(int slave, int error_descriptor, pid_t parent, char *const arguments[])
{
    // The bridge may be killed with an uncatchable signal such as SIGKILL, so
    // the sandboxed shell must die with it: otherwise it keeps holding the
    // slave tty open and the master-side drain never observes EOF.
    if (prctl(PR_SET_PDEATHSIG, SIGKILL) == -1)
    {
        report_child_error(error_descriptor, "prctl PDEATHSIG");
        _exit(EXIT_FAILURE);
    }

    if (getppid() != parent)
    {
        _exit(EXIT_FAILURE);
    }

    for (int descriptor = STDIN_FILENO; descriptor <= STDERR_FILENO; ++descriptor)
    {
        if (slave != descriptor && dup2(slave, descriptor) == -1)
        {
            report_child_error(error_descriptor, "dup2 slave tty");
            _exit(EXIT_FAILURE);
        }
    }

    if (slave > STDERR_FILENO && close(slave) == -1)
    {
        report_child_error(error_descriptor, "close slave tty");
        _exit(EXIT_FAILURE);
    }

    execvp(arguments[0], arguments);
    report_child_error(error_descriptor, "execvp bwrap");
    _exit(EXIT_FAILURE);
}

static int read_child_error(int descriptor, struct child_error *failure)
{
    unsigned char *cursor = (unsigned char *)failure;
    size_t remaining = sizeof(*failure);
    size_t received = 0;

    while (remaining > 0)
    {
        ssize_t count = read(descriptor, cursor + received, remaining);
        if (count == 0)
        {
            return received == 0 ? 0 : -1;
        }

        if (count == -1)
        {
            if (errno == EINTR)
            {
                continue;
            }

            return -1;
        }

        received += (size_t)count;
        remaining -= (size_t)count;
    }

    return 1;
}

static int wait_for_child(pid_t child, int *status)
{
    while (waitpid(child, status, 0) == -1)
    {
        if (errno != EINTR)
        {
            return report_error("waitpid");
        }
    }

    return EXIT_SUCCESS;
}

static int child_exit_code(int status)
{
    if (WIFEXITED(status))
    {
        return WEXITSTATUS(status);
    }

    if (WIFSIGNALED(status))
    {
        return 128 + WTERMSIG(status);
    }

    return EXIT_FAILURE;
}

static int run_bridge(int argc, char *argv[])
{
    if (argc < 5 || !is_separator(argv[3]) || argv[2][0] == '\0' || argv[4][0] == '\0')
    {
        return report_usage();
    }

    int slave = open(argv[2], O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (slave == -1)
    {
        return report_error("open slave tty");
    }

    if (validate_tty(slave, NULL) != EXIT_SUCCESS)
    {
        (void)close(slave);
        return EXIT_FAILURE;
    }

    int error_pipe[2];
    if (pipe2(error_pipe, O_CLOEXEC) == -1)
    {
        int error = errno;
        (void)close(slave);
        return report_error_code("pipe2", error);
    }

    pid_t parent = getpid();
    pid_t child = fork();
    if (child == -1)
    {
        int error = errno;
        (void)close(error_pipe[0]);
        (void)close(error_pipe[1]);
        (void)close(slave);
        return report_error_code("fork", error);
    }

    if (child == 0)
    {
        (void)close(error_pipe[0]);
        run_bridge_child(slave, error_pipe[1], parent, &argv[4]);
    }

    (void)close(error_pipe[1]);
    (void)close(slave);

    struct child_error failure;
    int error_result = read_child_error(error_pipe[0], &failure);
    int read_error = errno;
    (void)close(error_pipe[0]);

    if (error_result != 0)
    {
        int status;
        if (error_result > 0)
        {
            (void)report_error_code(failure.operation, failure.error);
        }
        else
        {
            (void)report_error_code("read child status", read_error == 0 ? EIO : read_error);
        }

        return wait_for_child(child, &status) == EXIT_SUCCESS ? child_exit_code(status) : EXIT_FAILURE;
    }

    static const char ready[] = "READY\n";
    if (write_all(STDERR_FILENO, ready, sizeof(ready) - 1) == -1)
    {
        int error = errno;
        (void)kill(child, SIGTERM);
        int status;
        (void)wait_for_child(child, &status);
        return report_error_code("write readiness", error);
    }

    int status;
    return wait_for_child(child, &status) == EXIT_SUCCESS ? child_exit_code(status) : EXIT_FAILURE;
}

static int validate_standard_ttys(void)
{
    dev_t device;
    if (validate_tty(STDIN_FILENO, &device) != EXIT_SUCCESS)
    {
        return EXIT_FAILURE;
    }

    for (int descriptor = STDOUT_FILENO; descriptor <= STDERR_FILENO; ++descriptor)
    {
        dev_t candidate;
        if (validate_tty(descriptor, &candidate) != EXIT_SUCCESS)
        {
            return EXIT_FAILURE;
        }

        if (candidate != device)
        {
            return report_error_code("standard descriptors do not share a tty", ENOTTY);
        }
    }

    return EXIT_SUCCESS;
}

static int establish_session(void)
{
    if (setsid() != -1)
    {
        return EXIT_SUCCESS;
    }

    int error = errno;
    if (error == EPERM && getsid(0) == getpid())
    {
        return EXIT_SUCCESS;
    }

    return report_error_code("setsid", error);
}

static int run_attach(int argc, char *argv[])
{
    if (argc < 4 || !is_separator(argv[2]) || argv[3][0] == '\0')
    {
        return report_usage();
    }

    if (validate_standard_ttys() != EXIT_SUCCESS || establish_session() != EXIT_SUCCESS)
    {
        return EXIT_FAILURE;
    }

    if (ioctl(STDIN_FILENO, TIOCSCTTY, 0) == -1)
    {
        return report_error("TIOCSCTTY");
    }

    if (tcsetpgrp(STDIN_FILENO, getpgrp()) == -1)
    {
        return report_error("tcsetpgrp");
    }

    execvp(argv[3], &argv[3]);
    return report_error("execvp command");
}

int main(int argc, char *argv[])
{
    if (argc < 2)
    {
        return report_usage();
    }

    if (strcmp(argv[1], "--bridge") == 0)
    {
        return run_bridge(argc, argv);
    }

    if (strcmp(argv[1], "--attach") == 0)
    {
        return run_attach(argc, argv);
    }

    return report_usage();
}
