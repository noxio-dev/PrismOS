/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - CLI mode implementation.
 */

#include "Cli.h"
#include "BootCheck.h"
#include "DiskScan.h"
#include "Installer.h"
#include "Layout.h"
#include "Util.h"
#include "Version.h"

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <iostream>
#include <sstream>

#include <signal.h>
#include <unistd.h>

namespace prism::cli {

namespace fs = std::filesystem;

/* ------------------------------------------------------------------ */
/* ANSI colors (disabled without a TTY or with NO_COLOR).              */
/* ------------------------------------------------------------------ */

namespace {

bool colorEnabled()
{
    static int enabled = -1;
    if (enabled < 0) {
        const char* nc = getenv("NO_COLOR");
        enabled = (isatty(STDOUT_FILENO) && !(nc && *nc)) ? 1 : 0;
    }
    return enabled == 1;
}

const char* C_RESET  () { return colorEnabled() ? "\033[0m"  : ""; }
const char* C_BOLD   () { return colorEnabled() ? "\033[1m"  : ""; }
const char* C_DIM    () { return colorEnabled() ? "\033[2m"  : ""; }
const char* C_RED    () { return colorEnabled() ? "\033[31m" : ""; }
const char* C_GREEN  () { return colorEnabled() ? "\033[32m" : ""; }
const char* C_YELLOW () { return colorEnabled() ? "\033[33m" : ""; }
const char* C_CYAN   () { return colorEnabled() ? "\033[36m" : ""; }

void outLine(const std::string& s) { std::cout << s << "\n"; }

/* Displays a status line: " · text" with a colored prefix. */
void status(const char* color, const std::string& tag, const std::string& text)
{
    outLine(std::string(" ") + color + C_BOLD() + "[" + tag + "]" + C_RESET()
            + " " + text);
}
void ok  (const std::string& s) { status(C_GREEN(),  " OK ", s); }
void warn(const std::string& s) { status(C_YELLOW(), "ATT ", s); }
void fail(const std::string& s) { status(C_RED(),   "FAIL", s); }
void note(const std::string& s) { status(C_CYAN(),  " •  ", s); }

std::string bold(const std::string& s) { return C_BOLD() + s + C_RESET(); }

namespace {

bool gStdinClosed = false;

}

/* ------------------------------------------------------------------ */
/* One-line progress bar.                                              */
/* ------------------------------------------------------------------ */

struct ProgressUi {
    bool tty = isatty(STDOUT_FILENO);
    bool lineActive = false;
    int lastPercent = -1;
    void render(int percent, const std::string& step)
    {
        if (!tty) {
            if (percent / 25 != lastPercent / 25) {   /* milestones only */
                lastPercent = percent;
                outLine("     " + std::to_string(percent) + "% - " + step);
            }
            return;
        }
        const int width = 30;
        int filled = width * percent / 100;
        std::string bar;
        for (int i = 0; i < width; ++i)
            bar += i < filled ? "█" : "░";
        std::cout << "\r\033[K";
        std::cout << "     " << C_CYAN() << bar << C_RESET() << " "
                  << std::setw(3) << percent << "%  " << step << std::flush;
        lineActive = true;
        lastPercent = percent;
    }

    void closeLine()
    {
        if (lineActive) {
            std::cout << "\r\033[K" << std::flush;
            lineActive = false;
        }
    }
};

/* ------------------------------------------------------------------ */
/* Interactive input.                                                  */
/* ------------------------------------------------------------------ */

std::string prompt(const std::string& question, const std::string& def)
{
    std::cout << "  " << bold(question);
    if (!def.empty()) std::cout << C_DIM() << " [" << def << "]" << C_RESET();
    std::cout << ": " << std::flush;

    std::string line;
    if (!std::getline(std::cin, line)) {
        /* Closed input (EOF): no interaction possible. */
        std::cin.clear();
        gStdinClosed = true;
        outLine("");
        return def;
    }
    /* Trim. */
    size_t a = line.find_first_not_of(" \t");
    size_t b = line.find_last_not_of(" \t");
    if (a == std::string::npos) return def;
    return line.substr(a, b - a + 1);
}

/* True if standard input has closed (EOF): any new prompt would be
 * an infinite loop, the wizard must stop. */
bool stdinClosed() { return gStdinClosed; }

uint64_t promptMiB(const std::string& question, uint64_t def,
                   uint64_t lo, uint64_t hi)
{
    for (;;) {
        if (stdinClosed())
            return def;               /* input closed: keep the default */
        std::string s = prompt(question, std::to_string(def));

        /* Input is in MiB (as the prompt says); suffixed forms like
         * "96M", "1G"... are also tolerated via parseSize, which
         * returns BYTES - hence the conversion back to MiB.
         * (Before: the value in bytes was compared against bounds in
         * MiB and everything, including just pressing Enter, was rejected.) */
        uint64_t v;
        if (s.find_first_not_of("0123456789") == std::string::npos) {
            v = s.empty() ? 0 : std::stoull(s);      /* MiB directly */
        } else {
            uint64_t bytes = parseSize(s);
            if (bytes == 0) {
                warn("invalid value: " + s);
                continue;
            }
            v = bytes / MIB;
            if (v == 0) {
                warn("too small (minimum 1 MiB): " + s);
                continue;
            }
        }
        if (v == 0) v = def;
        if (v < lo || v > hi) {
            warn("value out of range (" + std::to_string(lo) + "-"
                 + std::to_string(hi) + " MiB)");
            continue;
        }
        return v;
    }
}

/* ------------------------------------------------------------------ */
/* Banner.                                                             */
/* ------------------------------------------------------------------ */

void banner()
{
    if (!colorEnabled()) {
        outLine("PrismOS " PRISM_INSTALLER_VERSION " - installer");
        return;
    }
    outLine("");
    outLine(std::string(C_BOLD()) + C_CYAN() +
        "  ____       _                   ___  ____" + C_RESET());
    outLine(std::string(C_BOLD()) + C_CYAN() +
        " |  _ \\ _ __(_) __ _ _ __ _   _ / _ \\/ ___|" + C_RESET());
    outLine(std::string(C_BOLD()) + C_CYAN() +
        " | |_) | '__| |/ _` | '__| | | | | | \\___ \\" + C_RESET());
    outLine(std::string(C_BOLD()) + C_CYAN() +
        " |  __/| |  | | (_| | |  | |_| | |_| |___) |" + C_RESET());
    outLine(std::string(C_BOLD()) + C_CYAN() +
        " |_|   |_|  |_|\\__,_|_|   \\__, |\\___/|____/" + C_RESET());
    outLine(std::string(C_DIM()) +
        "                          |___/" + C_RESET());
    outLine("");
    outLine(std::string("  ") + PRISM_INSTALLER_TARGET + " - installer "
            + PRISM_INSTALLER_VERSION);
    outLine(std::string(C_DIM())
            + "  BIOS+UEFI bootloader · minimal ESP · kernel on the ext4 "
              "root" + C_RESET());
    outLine("");
}

/* ------------------------------------------------------------------ */
/* Plan display.                                                       */
/* ------------------------------------------------------------------ */

void printPlan(const PartitionPlan& plan, const TargetInfo& target,
               const SourceInfo& src)
{
    outLine("");
    note("Layout of disk " + target.path + " ("
         + humanSize(target.bytes) + ") :");
    outLine("");

    auto line = [](const char* tag, const std::string& desc,
                   uint64_t bytes, uint64_t total) {
        int width = 34;
        int filled = int(double(bytes) / double(total ? total : 1) * width);
        if (filled < 1) filled = 1;
        std::string bar;
        for (int i = 0; i < width; ++i) bar += i < filled ? "█" : "░";
        char buf[160];
        std::snprintf(buf, sizeof(buf), "  %-9s %s %-11s %s", tag,
                      bar.c_str(), humanSize(bytes).c_str(), desc.c_str());
        outLine(buf);
    };
    (void)src;

    line("GPT",    "partition table (start + end)", 34 * 512,
         target.bytes);
    line("ESP",    "FAT32 (FIRST partition): EFI/BOOT/BOOTX64.EFI",
         plan.espBytes(), target.bytes);
    line("boot",   "boot1 + BIOS bootloader (raw sectors)",
         plan.bootSectors * SECTOR, target.bytes);
    line("root",   "ext4 \"" + plan.rootLabel
                   + "\": kernel, Lua, loader.conf, mounted on /",
         plan.rootBytes(), target.bytes);

    outLine("");
}

} // namespace

/* ------------------------------------------------------------------ */
/* Argument parsing.                                                   */
/* ------------------------------------------------------------------ */

namespace {

const char* OPTS = R"(Options:
  -s, --source DIR       PrismOS build directory
                         (contains boot0.bin, boot1.bin, loader.bin,
                         esp/ and root/)
      --esp-dir DIR      overrides the ESP tree <source>/esp
                         (EFI/BOOT/BOOTX64.EFI)
      --root-dir DIR     overrides the ext4 root tree <source>/root
                         (boot/kernel/kernel, boot/lua, ...)
  -d, --disk PATH        target disk (/dev/sdX, /dev/nvme0n1) or image
      --image-size SIZE  size of the image to create if the file is absent
                         (e.g. 512M, 8G) - test/QEMU mode
      --esp-size MIB     size of the FAT32 ESP (default 36, min 34)
      --root-size MIB    size of the ext4 root partition (default: rest
                         of the disk)
      --esp-label L      FAT32 label (default PRISMESP)
      --root-label L     ext4 label (default PRISMROOT)
      --mke2fs PATH      mke2fs path (default: $PATH)
      --dry-run          validate everything, write nothing
      --check            READ-ONLY AUDIT of an already installed target:
                         --cli --check --disk /dev/sdX (or an image)
                         checks MBR/GPT, FAT32 ESP, EFI/BOOT/BOOTX64.EFI
                         (PE/COFF), the BIOS boot partition and the ext4
                         root (superblock, kernel)
      --efi-boot-entry   after installation, create a "PrismOS" entry
                         in the firmware menu (efibootmgr) - for
                         firmwares that treat the USB drive as a fixed
                         disk and do not try the removable path by default
      --efibootmgr PATH  efibootmgr path (default: $PATH; tests)
  -y, --yes              non-interactive mode (with --source and --disk)
      --cli              text interface (otherwise Qt graphical interface)
  -h, --help             this help
  -V, --version          version

Privileges: as soon as a real disk is targeted, the installer REQUESTS
                         administrator rights (desktop dialog in graphical
                         mode, sudo prompt in text mode) and relaunches
                         itself. Image files require no rights.
                         PRISM_INSTALLER_NO_ELEVATE=1 disables the
                         request.

Safety: installation COMPLETELY ERASES the target disk
                         (--check, for its part, only audits read-only).

Exit codes: 0 success · 1 usage · 2 validation refused ·
3 installation failure (or problems detected by --check) ·
4 administrator rights not obtained · 130 interrupted.
)";

} // namespace

std::string usageText()
{
    return std::string("Usage: " PRISM_INSTALLER_NAME
                       " [--cli] [options]\n\n") + OPTS;
}

Args parseArgs(int argc, char** argv)
{
    Args a;

    auto needValue = [&](int& i, const char* opt,
                         std::string& out) -> bool {
        if (i + 1 >= argc) {
            a.errors.push_back(std::string(opt) + " expects a value");
            return false;
        }
        out = argv[++i];
        return true;
    };

    for (int i = 1; i < argc; ++i) {
        std::string arg = argv[i];
        auto val = std::string{};

        if (arg == "--cli") { a.cli = true; }
        else if (arg == "--gui-self-test") { a.guiSelfTest = true; }
        else if (arg == "-y" || arg == "--yes") { a.yes = true; }
        else if (arg == "-h" || arg == "--help") { a.help = true; }
        else if (arg == "-V" || arg == "--version") { a.version = true; }
        else if (arg == "--check") { a.check = true; }
        else if (arg == "--dry-run") { a.options.dryRun = true; }
        else if (arg == "--efi-boot-entry") { a.options.efiBootEntry = true; }
        else if (arg == "-s" || arg == "--source") {
            if (needValue(i, "--source", val)) a.options.sourceDir = val;
        }
        else if (arg == "--esp-dir") {
            if (needValue(i, "--esp-dir", val)) a.options.espDir = val;
        }
        else if (arg == "--root-dir") {
            if (needValue(i, "--root-dir", val)) a.options.rootDir = val;
        }
        else if (arg == "-d" || arg == "--disk") {
            if (needValue(i, "--disk", val)) a.options.target = val;
        }
        else if (arg == "--image-size") {
            if (needValue(i, "--image-size", val)) {
                a.options.imageSize = parseSize(val);
                if (a.options.imageSize == 0)
                    a.errors.push_back("--image-size: invalid size: " + val);
            }
        }
        else if (arg == "--esp-size") {
            if (needValue(i, "--esp-size", val)) {
                a.options.espMiB = parseSize(val + "M") / MIB;
                if (a.options.espMiB == 0)
                    a.errors.push_back("--esp-size: invalid value: " + val);
            }
        }
        else if (arg == "--root-size") {
            if (needValue(i, "--root-size", val)) {
                a.options.rootMiB = parseSize(val + "M") / MIB;
                if (a.options.rootMiB == 0 && val != "0")
                    a.errors.push_back("--root-size: invalid value: " + val);
            }
        }
        else if (arg == "--esp-label") {
            if (needValue(i, "--esp-label", val)) a.options.espLabel = val;
        }
        else if (arg == "--root-label") {
            if (needValue(i, "--root-label", val)) a.options.rootLabel = val;
        }
        else if (arg == "--mke2fs") {
            if (needValue(i, "--mke2fs", val)) a.options.mke2fsPath = val;
        }
        else if (arg == "--efibootmgr") {
            if (needValue(i, "--efibootmgr", val))
                a.options.efibootmgrPath = val;
        }
        else {
            a.errors.push_back("unknown option: " + arg);
        }
    }
    return a;
}

/* ------------------------------------------------------------------ */
/* CLI wizard.                                                         */
/* ------------------------------------------------------------------ */

namespace {

/* ---- --check mode: audit of a target, writing nothing --------------- */

int runCheck(const Args& args)
{
    if (args.options.target.empty()) {
        fail("--check requires a target: --disk /dev/sdX (or an image)");
        std::cout << "\n" << usageText();
        return 1;
    }

    banner();
    note("read-only boot audit - no writes.");
    outLine("");

    auto rep = prism::check::auditTarget(args.options.target);

    for (const auto& l : rep.lines) note(l);
    outLine("");

    if (rep.ok()) {
        outLine(std::string("  ") + C_GREEN() + C_BOLD()
                + "✔ Bootable disk (BIOS and UEFI)." + C_RESET());
        outLine("");

        /* Secure Boot state of the audit machine: decisive for
         * knowing whether the unsigned chain CAN be loaded here.  -1
         * (undetermined: BIOS machine, container, test): silence. */
        int sb = prism::util::secureBootEnabled();
        if (sb == 1) {
            fail("Secure Boot is ENABLED on this machine: the firmware"
                 " will refuse to boot PrismOS (unsigned) - this is the"
                 " #1 cause of a \"compliant but non-bootable\" USB drive.");
            outLine("    disable it in the setup (F2/Esc -> Security"
                    " -> Secure Boot) and try again.");
            outLine("");
        } else if (sb == 0) {
            note("Secure Boot: disabled on this machine - the USB drive's"
                 " unsigned boot can be loaded.");
        }

        note("if the firmware still does not offer the USB drive at startup:");
        outLine("    - disable Secure Boot (the PrismOS boot is not"
                " signed);");
        outLine("    - open the boot menu (F12/Esc/F8...) and"
                " choose the USB drive's UEFI entry;");
        outLine("    - try another USB port (USB2 preferred);");
        outLine("    - as a last resort: --efi-boot-entry to force an"
                " entry in the firmware menu.");
        return 0;
    }

    for (const auto& p : rep.problems) fail(p);
    outLine("");
    outLine(std::string("  ") + C_RED() + C_BOLD()
            + "✘ " + std::to_string(rep.problems.size())
            + " problem(s): this disk is NOT bootable in its current state."
            + C_RESET());
    outLine("");
    return 3;
}

/* Looks for a plausible build directory (./build, ../build, .). */
std::vector<std::string> guessSources()
{
    std::vector<std::string> candidates;
    for (const char* c : {"build", "../build", ".", ".."}) {
        fs::path p(c);
        if (fs::exists(p / "boot0.bin") && fs::exists(p / "esp")
            && fs::exists(p / "root"))
            candidates.push_back(p.string());
    }
    return candidates;
}

int runAssistant(const Args& args)
{
    banner();

    InstallOptions opts = args.options;

    /* ---- Source ---- */
    if (opts.sourceDir.empty()) {
        if (args.yes) {
            fail("--yes requires --source (and --disk).");
            return 2;
        }
        auto guesses = guessSources();
        std::string def = guesses.empty() ? std::string("build")
                                          : guesses.front();
        if (!guesses.empty())
            note("build detected in: " + guesses.front());
        opts.sourceDir = prompt("PrismOS artifacts directory (make)", def);
    }

    Installer installer0(opts);
    SourceInfo src;
    try {
        src = installer0.loadSource();
        ok("source: " + opts.sourceDir + " - kernel "
           + humanSize(src.kernelBytes) + ", " + std::to_string(src.espFiles)
           + " ESP files (" + humanSize(src.espTreeBytes) + "), "
           + std::to_string(src.rootFiles) + " root files ("
           + humanSize(src.rootTreeBytes) + ")");
    } catch (const InstallError& ex) {
        fail(ex.what());
        if (!args.yes) {
            for (;;) {
                if (stdinClosed()) {
                    fail("standard input closed - rerun with --yes, "
                         "--source and --disk.");
                    return 2;
                }
                opts.sourceDir = prompt("Artifacts directory", "build");
                try {
                    Installer retry(opts);
                    src = retry.loadSource();
                    ok("source: " + opts.sourceDir);
                    break;
                } catch (const InstallError& e2) {
                    fail(e2.what());
                }
            }
        } else {
            return 2;
        }
    }

    /* ---- Target ---- */
    Installer probeInstaller(opts);
    TargetInfo target;
    auto tryTarget = [&](const std::string& path) -> bool {
        InstallOptions o = opts;
        o.target = path;
        Installer inst(o);
        try {
            target = inst.probeTarget();
        } catch (const InstallError& ex) {
            fail(ex.what());
            /* Most frequent cause: no rights to write to the disk.
             * Normally elevation is AUTOMATIC (requested at launch);
             * this point is only reached if it was disabled or if the
             * input is not interactive. */
            if (path.rfind("/dev/", 0) == 0 && ::geteuid() != 0
                && ::access(path.c_str(), W_OK) != 0) {
                note("writing a disk requires administrator "
                     "rights; run the wizard from an interactive "
                     "terminal (the privilege elevation request is "
                     "automatic there) or target an image file for a "
                     "risk-free test");
            }
            return false;
        }
        auto problems = inst.safetyProblems(target);
        if (!problems.empty()) {
            for (const auto& p : problems) fail(p);
            return false;
        }
        return true;
    };

    if (!opts.target.empty()) {
        if (!tryTarget(opts.target)) return 2;
        ok("target: " + target.description);
    } else if (args.yes) {
        fail("--yes requires --disk.");
        return 2;
    } else {
        auto disks = scanDisks();
        if (disks.empty()) {
            warn("no disk detected - you can target an image "
                 "file (test/QEMU mode).");
        } else {
            outLine("");
            note("disks detected:");
            for (size_t i = 0; i < disks.size(); ++i)
                outLine("  " + std::to_string(i + 1) + ") "
                        + disks[i].describe());
        }
        for (;;) {
            if (stdinClosed()) {
                fail("standard input closed - rerun with --yes, "
                     "--source and --disk.");
                return 2;
            }
            std::string choice = prompt(
                "Target disk (number, /dev/... or image path)",
                disks.empty() ? "prismos.img" : disks.front().dev);
            if (!choice.empty() && std::isdigit((unsigned char)choice[0])
                && choice.find_first_not_of("0123456789") == std::string::npos) {
                size_t idx = size_t(std::stoul(choice)) - 1;
                if (idx >= disks.size()) {
                    warn("invalid number");
                    continue;
                }
                choice = disks[idx].dev;
            }
            if (fs::path(choice).is_relative() && !fs::exists(choice)) {
                std::string sized = prompt(
                    "the file does not exist; size to create (e.g. 1G, empty "
                    "to abort)", "");
                if (sized.empty()) continue;
                uint64_t sz = parseSize(sized);
                if (sz == 0) { warn("invalid size"); continue; }
                opts.imageSize = sz;
            }
            opts.target = choice;
            if (tryTarget(choice)) break;
            opts.imageSize = args.options.imageSize;
        }
        ok("target: " + target.description);
    }

    /* ---- Sizes ---- */
    if (!args.yes) {
        outLine("");
        note("Sizes (Enter = default):");
        opts.espMiB = promptMiB("ESP (FAT32: UEFI stub only), MiB",
                                opts.espMiB, ESP_MIN_MIB, ESP_MAX_MIB);
        std::string rootSize = prompt(
            "ext4 root partition, MiB (Enter = all remaining space)", "");
        if (!rootSize.empty()) {
            uint64_t v = parseSize(rootSize + "M");
            if (v == 0) { warn("invalid value, rest of the disk used"); }
            else opts.rootMiB = v / MIB;
        }
    }

    /* ---- Plan + confirmation ---- */
    Installer installer(opts);
    PartitionPlan plan;
    try {
        plan = installer.computePlan(target, src);
    } catch (const InstallError& ex) {
        fail(ex.what());
        return 2;
    }
    printPlan(plan, target, src);

    if (!args.yes) {
        outLine(std::string("  ") + C_RED() + C_BOLD()
                + "WARNING: all content of " + target.path
                + " will be PERMANENTLY erased." + C_RESET());
        outLine("");
        std::string confirm = prompt(
            "Type \"" + target.path + "\" (or the image file name) "
            "to confirm", "");
        if (confirm != target.path && confirm != fs::path(target.path).filename().string()) {
            outLine("");
            note("confirmation missing - aborting without any writes.");
            return 0;
        }
    }

    outLine("");
    if (opts.dryRun)
        note(std::string(C_BOLD()) + "DRY RUN MODE" + C_RESET()
             + ": full validation, no writes.");
    outLine("");

    /* ---- Execution ---- */
    ProgressUi ui;
    prism::ProgressHooks hooks;
    hooks.info = [&](const std::string& m) {
        ui.closeLine();
        ok(m);
    };
    hooks.detail = [&](const std::string& m) {
        ui.closeLine();
        outLine(std::string("        ") + C_DIM() + m + C_RESET());
    };
    hooks.progress = [&](int p, const std::string& step) {
        ui.render(p, step);
    };

    try {
        InstallSummary summary = installer.run(hooks);
        ui.closeLine();
        outLine("");
        if (summary.dryRun) {
            note("dry run complete - rerun without --dry-run to "
                 "install for real.");
        } else {
            outLine(std::string("  ") + C_GREEN() + C_BOLD()
                    + "✔ Installation succeeded."
                    + C_RESET());
            outLine("");
            outLine("  Disk " + target.path + " boots in BIOS "
                    "as well as UEFI:");
            outLine("    - boot0 (MBR) -> boot1 -> bootloader (BIOS)");
            outLine("    - EFI/BOOT/BOOTX64.EFI (UEFI)");
            outLine("    - kernel: " + humanSize(summary.kernelBytes)
                    + " on the ext4 root (/boot/kernel/kernel)");
            outLine("    - ext4 \"" + plan.rootLabel + "\": "
                    + humanSize(plan.rootBytes()) + ", "
                    + std::to_string(summary.rootFiles) + " files seeded");
            if (target.isImage)
                outLine("  Test: qemu-system-x86_64 -drive file="
                        + target.path + ",format=raw -serial stdio");
        }
        outLine("");
        return 0;
    } catch (const InstallError& ex) {
        ui.closeLine();
        outLine("");
        fail(ex.what());
        outLine("");
        return 3;
    } catch (const std::exception& ex) {
        ui.closeLine();
        outLine("");
        fail(std::string("unexpected error: ") + ex.what());
        outLine("");
        return 3;
    }
}

} // namespace

int cliMain(const Args& args)
{
    /* Ctrl-C: a clean message rather than a silent abort in the
     * middle of a write. */
    struct sigaction sa{};
    sa.sa_handler = [](int) {
        const char msg[] = "\nInterrupted - aborting.\n";
        ssize_t r = ::write(STDERR_FILENO, msg, sizeof(msg) - 1);
        (void)r;
        _exit(130);
    };
    sigaction(SIGINT, &sa, nullptr);

    if (args.help) {
        std::cout << usageText();
        return 0;
    }
    if (args.version) {
        std::cout << PRISM_INSTALLER_NAME " " PRISM_INSTALLER_VERSION
                     " (" PRISM_INSTALLER_TARGET ")\n";
        return 0;
    }
    if (!args.errors.empty()) {
        for (const auto& e : args.errors) fail(e);
        std::cout << "\n" << usageText();
        return 1;
    }

    if (args.check)
        return runCheck(args);

    return runAssistant(args);
}

} // namespace prism::cli
