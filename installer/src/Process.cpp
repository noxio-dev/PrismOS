/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - program execution implementation.
 */

#include "Process.h"

#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <stdexcept>
#include <sys/wait.h>
#include <unistd.h>

#include <cstdlib>
#include <ctime>
#include <filesystem>
#include <sstream>

extern char** environ;

namespace prism {

namespace {

std::string joinCommandLine(const std::string& prog,
                             const std::vector<std::string>& args)
{
    std::ostringstream os;
    os << prog;
    for (const auto& a : args) os << " " << a;
    return os.str();
}

/* Non-blocking read of two pipes with polling (global timeout). */
void drainPipes(int outFd, int errFd, std::string& out, std::string& err,
                unsigned timeoutSeconds, bool& timedOut)
{
    int flagsOut = fcntl(outFd, F_GETFL, 0);
    if (flagsOut >= 0) fcntl(outFd, F_SETFL, flagsOut | O_NONBLOCK);
    int flagsErr = fcntl(errFd, F_GETFL, 0);
    if (flagsErr >= 0) fcntl(errFd, F_SETFL, flagsErr | O_NONBLOCK);

    char buf[4096];
    time_t start = time(nullptr);
    bool outOpen = true, errOpen = true;

    while (outOpen || errOpen) {
        if (timeoutSeconds && time(nullptr) - start > time_t(timeoutSeconds)) {
            timedOut = true;
            return;
        }
        bool progressed = false;
        if (outOpen) {
            ssize_t n = ::read(outFd, buf, sizeof(buf));
            if (n > 0) { out.append(buf, size_t(n)); progressed = true; }
            else if (n == 0) outOpen = false;
            else if (errno != EAGAIN && errno != EINTR) outOpen = false;
        }
        if (errOpen) {
            ssize_t n = ::read(errFd, buf, sizeof(buf));
            if (n > 0) { err.append(buf, size_t(n)); progressed = true; }
            else if (n == 0) errOpen = false;
            else if (errno != EAGAIN && errno != EINTR) errOpen = false;
        }
        if (!progressed)
            usleep(20000);       /* 20 ms */
    }
}

ProcessResult execProgram(const std::string& program,
                          const std::vector<std::string>& args,
                          const std::map<std::string, std::string>& env,
                          unsigned timeoutSeconds)
{
    int outPipe[2], errPipe[2];
    if (pipe(outPipe) != 0 || pipe(errPipe) != 0)
        throw std::runtime_error("pipe() failed");

    std::string cmdline = joinCommandLine(program, args);

    pid_t pid = fork();
    if (pid < 0)
        throw std::runtime_error("fork() failed");

    if (pid == 0) {
        /* ---- child process ---- */
        ::close(outPipe[0]);
        ::close(errPipe[0]);
        dup2(outPipe[1], STDOUT_FILENO);
        dup2(errPipe[1], STDERR_FILENO);
        ::close(outPipe[1]);
        ::close(errPipe[1]);

        for (const auto& kv : env)
            setenv(kv.first.c_str(), kv.second.c_str(), 1);

        std::vector<char*> argv;
        argv.push_back(const_cast<char*>(program.c_str()));
        for (const auto& a : args)
            argv.push_back(const_cast<char*>(a.c_str()));
        argv.push_back(nullptr);

        execv(program.c_str(), argv.data());
        _exit(127);               /* program not found */
    }

    /* ---- parent process ---- */
    ::close(outPipe[1]);
    ::close(errPipe[1]);

    ProcessResult r;
    r.commandLine = cmdline;

    bool timedOut = false;
    drainPipes(outPipe[0], errPipe[0], r.stdoutText, r.stderrText,
               timeoutSeconds, timedOut);

    if (timedOut) {
        kill(pid, SIGKILL);
        waitpid(pid, nullptr, 0);
        ::close(outPipe[0]);
        ::close(errPipe[0]);
        r.exitCode = -1;
        r.stderrText += "\n[timeout exceeded]";
        return r;
    }

    int status = 0;
    while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {}

    ::close(outPipe[0]);
    ::close(errPipe[0]);

    if (WIFEXITED(status))
        r.exitCode = WEXITSTATUS(status);
    else if (WIFSIGNALED(status))
        r.exitCode = 128 + WTERMSIG(status);

    if (r.exitCode == 127)
        throw std::runtime_error("program not found: " + program);
    return r;
}

} // namespace

std::string which(const std::string& name)
{
    if (name.find('/') != std::string::npos)
        return std::filesystem::exists(name) ? name : "";

    const char* pathEnv = getenv("PATH");
    if (!pathEnv) return "";

    std::istringstream is(pathEnv);
    std::string dir;
    while (std::getline(is, dir, ':')) {
        if (dir.empty()) continue;
        std::string cand = dir + "/" + name;
        if (access(cand.c_str(), X_OK) == 0)
            return cand;
    }
    return "";
}

ProcessResult runProcess(const std::string& program,
                         const std::vector<std::string>& args,
                         const std::map<std::string, std::string>& env,
                         unsigned timeoutSeconds)
{
    return execProgram(program, args, env, timeoutSeconds);
}

ProcessResult runProcessPath(const std::string& name,
                             const std::vector<std::string>& args,
                             const std::map<std::string, std::string>& env,
                             unsigned timeoutSeconds)
{
    std::string full = which(name);
    if (full.empty())
        throw std::runtime_error(
            "program not found in PATH: " + name +
            " (install the corresponding package)");
    return execProgram(full, args, env, timeoutSeconds);
}

} // namespace prism
