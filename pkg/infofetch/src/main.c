/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * infofetch -- PrismOS system information
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/utsname.h>

#define RESET   "\033[0m"
#define BOLD    "\033[1m"
#define DIM     "\033[2m"

#define CYAN    "\033[36m"
#define BLUE    "\033[34m"
#define PURPLE  "\033[35m"
#define WHITE   "\033[37m"
#define GRAY    "\033[90m"

// Fixed width reserved for the logo block (adjusted to prevent shifts)
#define LOGO_WIDTH 14

static const char *logo[] = {
    "      /\\    ",
    "     /  \\   ",
    "    / /\\ \\  ",
    "   / /  \\ \\ ",
    "  /_/____\\_\\"
};

static void print_uptime(char *buffer, size_t size)
{
    FILE *fp = fopen("/proc/uptime", "r");
    double value = 0.0;

    if (fp) {
        if (fscanf(fp, "%lf", &value) != 1)
            value = 0.0;
        fclose(fp);
    }

    unsigned long sec = (unsigned long)value;
    unsigned long days = sec / 86400;
    unsigned long hours = (sec % 86400) / 3600;
    unsigned long mins = (sec % 3600) / 60;

    if (days)
        snprintf(buffer, size, "%lu days, %lu hrs, %lu mins", days, hours, mins);
    else if (hours)
        snprintf(buffer, size, "%lu hrs, %lu mins", hours, mins);
    else
        snprintf(buffer, size, "%lu mins", mins);
}

static void print_memory(char *buffer, size_t size)
{
    FILE *fp = fopen("/proc/meminfo", "r");
    unsigned long total = 0;
    unsigned long available = 0;

    if (fp) {
        char line[128];
        while (fgets(line, sizeof(line), fp)) {
            if (sscanf(line, "MemTotal: %lu kB", &total) == 1)
                continue;
            if (sscanf(line, "MemAvailable: %lu kB", &available) == 1)
                continue;
        }
        fclose(fp);
    }

    if (!total) {
        snprintf(buffer, size, "Unavailable");
        return;
    }

    if (available > total)
        available = total;

    unsigned long used = total - available;

    snprintf(buffer, size, "%lu MiB / %lu MiB (%lu%%)",
             used / 1024,
             total / 1024,
             used * 100 / total);
}

static void print_cpu(char *buffer, size_t size)
{
    FILE *fp = fopen("/proc/cpuinfo", "r");
    if (!fp) {
        snprintf(buffer, size, "Unknown");
        return;
    }

    char line[256];
    int found = 0;

    while (fgets(line, sizeof(line), fp)) {
        if (strncmp(line, "model name", 10) == 0 ||
            strncmp(line, "Hardware", 8) == 0) {
            char *colon = strchr(line, ':');
            if (colon) {
                colon++;
                while (*colon == ' ' || *colon == '\t')
                    colon++;
                colon[strcspn(colon, "\r\n")] = '\0';
                snprintf(buffer, size, "%s", colon);
                found = 1;
                break;
            }
        }
    }
    fclose(fp);

    if (!found)
        snprintf(buffer, size, "Unknown");
}

static void print_row(const char *logo_line, const char *color, const char *label, const char *value)
{
    if (logo_line && color) {
        // Displays the logo in its color and pads it with spaces to reach LOGO_WIDTH
        printf("%s%-*s" RESET "   ", color, LOGO_WIDTH, logo_line);
    } else {
        // Empty lines below the logo to maintain alignment
        printf("%-*s   ", LOGO_WIDTH, "");
    }

    if (label && value) {
        printf(BOLD PURPLE "%-8s" RESET " " GRAY "•" RESET " " WHITE "%s\n", label, value);
    } else if (label) {
        printf("%s\n" RESET, label);
    } else {
        printf("\n");
    }
}

static void print_fetch(const char *user,
                        const char *host,
                        const struct utsname *uts,
                        const char *uptime,
                        const char *cpu,
                        const char *memory)
{
    char title_buf[128];
    snprintf(title_buf, sizeof(title_buf), CYAN "%s" GRAY "@" PURPLE "%s", user, host);

    printf("\n");

    print_row(logo[0], CYAN, title_buf, NULL);
    print_row(logo[1], CYAN, "-----------", NULL);
    print_row(logo[2], BLUE, "OS", "PrismOS");
    print_row(logo[3], BLUE, "Kernel", uts->release);
    print_row(logo[4], PURPLE, "Uptime", uptime);
    print_row(NULL, NULL, "Arch", uts->machine);
    print_row(NULL, NULL, "CPU", cpu);
    print_row(NULL, NULL, "RAM", memory);
    
    printf("\n");
}

int main(void)
{
    struct utsname uts;
    char uptime[96];
    char memory[96];
    char cpu[256];
    char hostname[128] = "prismos";

    const char *user = getenv("USER");
    if (!user || !*user)
        user = "root";

    if (uname(&uts) != 0) {
        memset(&uts, 0, sizeof(uts));
        strcpy(uts.sysname, "PrismOS");
        strcpy(uts.release, "Unknown");
        strcpy(uts.machine, "Unknown");
    } else if (uts.nodename[0]) {
        snprintf(hostname, sizeof(hostname), "%s", uts.nodename);
    }

    print_uptime(uptime, sizeof(uptime));
    print_memory(memory, sizeof(memory));
    print_cpu(cpu, sizeof(cpu));

    print_fetch(user, hostname, &uts, uptime, cpu, memory);

    return 0;
}