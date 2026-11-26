#include <mach-o/dyld.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

// Use the project-owned credential resolver bundled with this application.
int main(int argc, char **argv) {
    (void)argc;
    uint32_t size = 0;
    _NSGetExecutablePath(NULL, &size);
    char *raw = malloc(size);
    if (!raw || _NSGetExecutablePath(raw, &size) != 0) {
        fputs("Private Archive: cannot locate the application\n", stderr);
        return 127;
    }
    char *path = realpath(raw, NULL);
    free(raw);
    if (!path) {
        perror("Private Archive: application path");
        return 127;
    }
    char *slash = strrchr(path, '/');
    if (!slash) {
        free(path);
        return 127;
    }
    *slash = '\0';
    const char suffix[] = "/private-archive-gui";
    char *binary = malloc(strlen(path) + sizeof(suffix));
    if (!binary) {
        free(path);
        return 127;
    }
    strcpy(binary, path);
    strcat(binary, suffix);
    const char session_suffix[] = "/../Resources/private-archive-session.sh";
    char *session = malloc(strlen(path) + sizeof(session_suffix));
    char **args = calloc((size_t)argc + 3, sizeof(char *));
    if (!session || !args) {
        free(path);
        free(binary);
        free(session);
        free(args);
        return 127;
    }
    strcpy(session, path);
    strcat(session, session_suffix);
    free(path);
    args[0] = "/bin/bash";
    args[1] = session;
    args[2] = binary;
    for (int i = 1; i < argc; i++) args[i + 2] = argv[i];
    execv(args[0], args);
    perror("Private Archive: launch GUI");
    free(binary);
    free(session);
    free(args);
    return 127;
}
