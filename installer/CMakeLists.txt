#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# prism-installer - build system (Qt Widgets, C++20).
#
#   cmake -B build -DCMAKE_BUILD_TYPE=Release
#   cmake --build build
#
# Qt 6 (recommended) or Qt 5 is detected automatically; the
# CMAKE_PREFIX_PATH variable can point to a local Qt installation:
#   cmake -B build -DCMAKE_PREFIX_PATH=$HOME/Qt/6.8.3/gcc_64
#

cmake_minimum_required(VERSION 3.16)

project(prism-installer
    VERSION 1.0.0
    DESCRIPTION "PrismOS installer (BIOS + UEFI, minimal ESP, kernel on the ext4 root)"
    LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

set(CMAKE_AUTOMOC ON)

if (NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release CACHE STRING "" FORCE)
endif()

# ---- Qt ----------------------------------------------------------------

find_package(Qt6 6.5 QUIET COMPONENTS Widgets)
if (NOT Qt6_FOUND)
    find_package(Qt5 5.15 QUIET COMPONENTS Widgets)
    if (NOT Qt5_FOUND)
        message(FATAL_ERROR
            "Qt was not found.  Install qt6-base-dev (Debian/Ubuntu:"
            " qt6-base-dev / qt6-base-dev-tools; Arch: qt6-base), or"
            " specify -DCMAKE_PREFIX_PATH=/path/to/qt.")
    endif()
    set(PRISM_QT_TARGET Qt5::Widgets)
    set(PRISM_QT_VERSION "Qt5")
else()
    set(PRISM_QT_TARGET Qt6::Widgets)
    set(PRISM_QT_VERSION "Qt6")
endif()

message(STATUS "prism-installer: graphical interface ${PRISM_QT_VERSION}")

# ---- Binary --------------------------------------------------------------

add_executable(prism-installer
    src/main.cpp
    src/Cli.cpp
    src/Elevate.cpp
    src/Gui.cpp
    src/Installer.cpp
    src/Gpt.cpp
    src/Fat32.cpp
    src/DiskScan.cpp
    src/Process.cpp
    src/Ext4.cpp
    src/Verifier.cpp
    src/Pe.cpp
    src/BootCheck.cpp
    src/Util.cpp
)

target_include_directories(prism-installer PRIVATE src)
target_link_libraries(prism-installer PRIVATE ${PRISM_QT_TARGET})

# No warnings expected: treat them as errors on project files (not on
# system headers).
if (CMAKE_CXX_COMPILER_ID MATCHES "GNU|Clang")
    target_compile_options(prism-installer PRIVATE
        -Wall -Wextra -Wno-unused-parameter)
endif()

install(TARGETS prism-installer RUNTIME DESTINATION bin)
