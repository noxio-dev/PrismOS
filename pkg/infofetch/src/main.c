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

#define WIDTH     64
#define LOGO_WIDTH 14

static const char *logo[] = {
    "      /\\",
    "     /  \\",
    "    / /\\ \\",
    "   / /  \\ \\",
    "  /_/____\\_\\"
};

static void print_border(const char *left,
                         const char *fill,
                         const char *right)
{
    printf("%s", left);

    for (int i = 0; i < WIDTH; i++)
        printf("%s", fill);

    printf("%s\n", right);
}

static void print_center(const char *text, size_t len)
{
    size_t padding = len < WIDTH ? (WIDTH - len) / 2 : 0;

    printf("│");
    for (size_t i = 0; i < padding; i++)
        putchar(' ');

    printf("%s", text);

    for (size_t i = padding + len; i < WIDTH; i++)
        putchar(' ');

    printf("│\n");
}

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
        snprintf(buffer, size, "%lu days, %lu hrs, %lu mins",
                 days, hours, mins);
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

static const char *gpu_vendor(unsigned vendor)
{
    switch (vendor) {
    case 0x10de: return "NVIDIA";
    case 0x1002: return "AMD";
    case 0x8086: return "Intel";
    case 0x1af4: return "Red Hat virtio";
    case 0x1234: return "QEMU";
    case 0x15ad: return "VMware";
    default:     return "Unknown vendor";
    }
}

/* Display controllers (PCI class 03) from /proc/pci; up to max entries. */
static int find_gpus(char gpus[][64], int max)
{
    FILE *fp = fopen("/proc/pci", "r");
    char line[128];
    int n = 0;

    if (!fp)
        return 0;

    while (n < max && fgets(line, sizeof(line), fp)) {
        unsigned bus, dev, func, cls, ven, did;

        if (sscanf(line, "%x:%x.%u %x %x:%x",
                   &bus, &dev, &func, &cls, &ven, &did) != 6)
            continue;
        if ((cls >> 8) != 0x03)
            continue;

        snprintf(gpus[n], 64, "%s [%04x:%04x]", gpu_vendor(ven), ven, did);
        n++;
    }

    fclose(fp);
    return n;
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

static void print_row(const char *logo_line,
                      const char *logo_color,
                      const char *label,
                      const char *value)
{
    printf("│  ");

    if (logo_line && logo_color)
        printf("%s%-*s" RESET, logo_color, LOGO_WIDTH, logo_line);
    else
        printf("%-*s", LOGO_WIDTH, "");

    printf("   ");

    if (label && value) {
        printf(BOLD PURPLE "%-8s" RESET " "
               GRAY "•" RESET " "
               WHITE "%-34.34s" RESET,
               label, value);
    } else if (label) {
        printf("%-45.45s", label);
    } else {
        printf("%45s", "");
    }

    printf("│\n");
}

static void print_fetch(const char *user,
                        const char *host,
                        const struct utsname *uts,
                        const char *uptime,
                        const char *cpu,
                        const char *memory,
                        char gpus[][64], int gpu_count)
{
    char identity[128];

    snprintf(identity, sizeof(identity), "%s@%s", user, host);

    printf("\n");

    print_border("╭", "─", "╮");

    printf("│%*s│\n", WIDTH, "");

    print_center(BOLD CYAN "PrismOS System Information" RESET, 26);
    print_center(GRAY "System overview" RESET, 15);

    printf("│%*s│\n", WIDTH, "");

    print_border("├", "─", "┤");

    print_row(logo[0], CYAN, identity, NULL);
    print_row(logo[1], CYAN, "-----------", NULL);
    print_row(logo[2], BLUE, "OS", "PrismOS");
    print_row(logo[3], BLUE, "Kernel", uts->release);
    print_row(logo[4], PURPLE, "Uptime", uptime);
    print_row(NULL, NULL, "Arch", uts->machine);
    print_row(NULL, NULL, "CPU", cpu);
    for (int i = 0; i < gpu_count; i++)
        print_row(NULL, NULL, "GPU", gpus[i]);
    print_row(NULL, NULL, "RAM", memory);

    printf("│%*s│\n", WIDTH, "");

    print_border("╰", "─", "╯");

    printf("\n");
}

int main(void)
{
    struct utsname uts;

    char uptime[96];
    char memory[96];
    char cpu[256];
    char gpus[4][64];
    int gpu_count;
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
    gpu_count = find_gpus(gpus, 4);

    print_fetch(user, hostname, &uts, uptime, cpu, memory, gpus, gpu_count);

    return 0;
}