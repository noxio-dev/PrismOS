/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */

#ifndef EFI_H
#define EFI_H

#include <stdint.h>
#include <stddef.h>

typedef uint64_t UINTN;
typedef int64_t  INTN;
typedef uint16_t CHAR16;
typedef UINTN    EFI_STATUS;
typedef void    *EFI_HANDLE;
typedef void    *EFI_EVENT;
typedef uint64_t EFI_PHYSICAL_ADDRESS;

#define EFI_ERR(n)           (0x8000000000000000ULL | (n))
#define EFI_SUCCESS          0
#define EFI_BUFFER_TOO_SMALL EFI_ERR(5)
#define EFI_NOT_READY        EFI_ERR(6)
#define EFI_NOT_FOUND        EFI_ERR(14)
#define EFI_ERROR(s)         ((INTN)(s) < 0)

typedef struct { uint32_t d1; uint16_t d2, d3; uint8_t d4[8]; } EFI_GUID;

typedef struct {
    uint64_t signature;
    uint32_t revision, header_size, crc32, reserved;
} EFI_TABLE_HEADER;

/* ---- text I/O ---------------------------------------------------------- */

typedef struct { uint16_t scan_code; CHAR16 unicode_char; } EFI_INPUT_KEY;

typedef struct EFI_SIMPLE_TEXT_INPUT_PROTOCOL {
    EFI_STATUS (*reset)(struct EFI_SIMPLE_TEXT_INPUT_PROTOCOL *, uint8_t);
    EFI_STATUS (*read_key_stroke)(struct EFI_SIMPLE_TEXT_INPUT_PROTOCOL *, EFI_INPUT_KEY *);
    EFI_EVENT wait_for_key;
} EFI_SIMPLE_TEXT_INPUT_PROTOCOL;

typedef struct {
    int32_t max_mode, mode, attribute, cursor_column, cursor_row;
    uint8_t cursor_visible;
} EFI_SIMPLE_TEXT_OUTPUT_MODE;

typedef struct EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL {
    EFI_STATUS (*reset)(struct EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *, uint8_t);
    EFI_STATUS (*output_string)(struct EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *, CHAR16 *);
    EFI_STATUS (*test_string)(struct EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *, CHAR16 *);
    EFI_STATUS (*query_mode)(struct EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *, UINTN, UINTN *, UINTN *);
    EFI_STATUS (*set_mode)(struct EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *, UINTN);
    EFI_STATUS (*set_attribute)(struct EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *, UINTN);
    EFI_STATUS (*clear_screen)(struct EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *);
    EFI_STATUS (*set_cursor_position)(struct EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *, UINTN, UINTN);
    EFI_STATUS (*enable_cursor)(struct EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *, uint8_t);
    EFI_SIMPLE_TEXT_OUTPUT_MODE *mode;
} EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL;

/* scan codes of EFI_INPUT_KEY */
#define SCAN_UP     0x01
#define SCAN_DOWN   0x02
#define SCAN_RIGHT  0x03
#define SCAN_LEFT   0x04
#define SCAN_HOME   0x05
#define SCAN_END    0x06
#define SCAN_INSERT 0x07
#define SCAN_DELETE 0x08
#define SCAN_PGUP   0x09
#define SCAN_PGDN   0x0A
#define SCAN_F1     0x0B     /* F1..F10 = 0x0B..0x14 */
#define SCAN_F11    0x15
#define SCAN_F12    0x16
#define SCAN_ESC    0x17

/* ---- memory ------------------------------------------------------------- */

typedef struct {
    uint32_t type;
    uint32_t pad;
    uint64_t physical_start;
    uint64_t virtual_start;
    uint64_t number_of_pages;
    uint64_t attribute;
} EFI_MEMORY_DESCRIPTOR;

enum { AllocateAnyPages, AllocateMaxAddress, AllocateAddress };
enum { EfiReservedMemoryType, EfiLoaderCode, EfiLoaderData, EfiBootServicesCode,
       EfiBootServicesData, EfiRuntimeServicesCode, EfiRuntimeServicesData,
       EfiConventionalMemory, EfiUnusableMemory, EfiACPIReclaimMemory,
       EfiACPIMemoryNVS, EfiMemoryMappedIO, EfiMemoryMappedIOPortSpace, EfiPalCode,
       EfiPersistentMemory, EfiUnacceptedMemoryType };

/* ---- services ----------------------------------------------------------- */

typedef struct {
    EFI_TABLE_HEADER hdr;
    void *raise_tpl, *restore_tpl;
    EFI_STATUS (*allocate_pages)(int, int, UINTN, EFI_PHYSICAL_ADDRESS *);
    EFI_STATUS (*free_pages)(EFI_PHYSICAL_ADDRESS, UINTN);
    EFI_STATUS (*get_memory_map)(UINTN *, EFI_MEMORY_DESCRIPTOR *, UINTN *, UINTN *, uint32_t *);
    EFI_STATUS (*allocate_pool)(int, UINTN, void **);
    EFI_STATUS (*free_pool)(void *);
    void *create_event, *set_timer, *wait_for_event, *signal_event, *close_event, *check_event;
    void *install_protocol_interface, *reinstall_protocol_interface, *uninstall_protocol_interface;
    EFI_STATUS (*handle_protocol)(EFI_HANDLE, EFI_GUID *, void **);
    void *reserved, *register_protocol_notify;
    EFI_STATUS (*locate_handle)(int, EFI_GUID *, void *, UINTN *, EFI_HANDLE *);
    void *locate_device_path;
    void *install_configuration_table, *load_image, *start_image, *exit, *unload_image;
    EFI_STATUS (*exit_boot_services)(EFI_HANDLE, UINTN);
    void *get_next_monotonic_count;
    EFI_STATUS (*stall)(UINTN);
    EFI_STATUS (*set_watchdog_timer)(UINTN, uint64_t, UINTN, CHAR16 *);
    void *connect_controller, *disconnect_controller, *open_protocol, *close_protocol;
    void *open_protocol_information, *protocols_per_handle;
    EFI_STATUS (*locate_handle_buffer)(int, EFI_GUID *, void *, UINTN *, EFI_HANDLE **);
    EFI_STATUS (*locate_protocol)(EFI_GUID *, void *, void **);
} EFI_BOOT_SERVICES;

typedef struct {
    EFI_TABLE_HEADER hdr;
    void *get_time, *set_time, *get_wakeup_time, *set_wakeup_time;
    void *set_virtual_address_map, *convert_pointer;
    void *get_variable, *get_next_variable_name, *set_variable, *get_next_high_monotonic_count;
    void (*reset_system)(int, EFI_STATUS, UINTN, void *);
} EFI_RUNTIME_SERVICES;

typedef struct { EFI_GUID guid; void *table; } EFI_CONFIGURATION_TABLE;

typedef struct {
    EFI_TABLE_HEADER hdr;
    CHAR16 *firmware_vendor;
    uint32_t firmware_revision;
    EFI_HANDLE console_in_handle;
    EFI_SIMPLE_TEXT_INPUT_PROTOCOL *con_in;
    EFI_HANDLE console_out_handle;
    EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *con_out;
    EFI_HANDLE standard_error_handle;
    EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL *std_err;
    EFI_RUNTIME_SERVICES *runtime_services;
    EFI_BOOT_SERVICES *boot_services;
    UINTN number_of_table_entries;
    EFI_CONFIGURATION_TABLE *configuration_table;
} EFI_SYSTEM_TABLE;

/* ---- loaded image, file system ---------------------------------------------- */

/* LocateHandle search types */
enum { ByAllHandles, ByRegisterNotify, ByProtocol };

typedef struct {
    uint32_t media_id;
    uint8_t  removable_media, media_present, logical_partition, read_only;
    uint8_t  write_protected, cacheable, erase_polarity;
    uint32_t block_size;
    uint32_t io_align;
    uint64_t last_block;
} EFI_BLOCK_IO_MEDIA;

typedef struct EFI_BLOCK_IO_PROTOCOL {
    uint64_t revision;
    EFI_BLOCK_IO_MEDIA *media;
    EFI_STATUS (*reset)(struct EFI_BLOCK_IO_PROTOCOL *, uint8_t);
    EFI_STATUS (*read_blocks)(struct EFI_BLOCK_IO_PROTOCOL *, uint32_t, uint64_t, UINTN, void *);
    EFI_STATUS (*write_blocks)(struct EFI_BLOCK_IO_PROTOCOL *, uint32_t, uint64_t, UINTN, void *);
    EFI_STATUS (*flush_blocks)(struct EFI_BLOCK_IO_PROTOCOL *);
} EFI_BLOCK_IO_PROTOCOL, EFI_BLOCK_IO;

typedef struct {
    uint32_t revision;
    EFI_HANDLE parent_handle;
    EFI_SYSTEM_TABLE *system_table;
    EFI_HANDLE device_handle;
} EFI_LOADED_IMAGE_PROTOCOL;           /* only the leading fields we read */

typedef struct EFI_FILE_PROTOCOL {
    uint64_t revision;
    EFI_STATUS (*open)(struct EFI_FILE_PROTOCOL *, struct EFI_FILE_PROTOCOL **, CHAR16 *, uint64_t, uint64_t);
    EFI_STATUS (*close)(struct EFI_FILE_PROTOCOL *);
    EFI_STATUS (*delete)(struct EFI_FILE_PROTOCOL *);
    EFI_STATUS (*read)(struct EFI_FILE_PROTOCOL *, UINTN *, void *);
    EFI_STATUS (*write)(struct EFI_FILE_PROTOCOL *, UINTN *, void *);
    EFI_STATUS (*get_position)(struct EFI_FILE_PROTOCOL *, uint64_t *);
    EFI_STATUS (*set_position)(struct EFI_FILE_PROTOCOL *, uint64_t);
    EFI_STATUS (*get_info)(struct EFI_FILE_PROTOCOL *, EFI_GUID *, UINTN *, void *);
} EFI_FILE_PROTOCOL;

typedef struct EFI_SIMPLE_FILE_SYSTEM_PROTOCOL {
    uint64_t revision;
    EFI_STATUS (*open_volume)(struct EFI_SIMPLE_FILE_SYSTEM_PROTOCOL *, EFI_FILE_PROTOCOL **);
} EFI_SIMPLE_FILE_SYSTEM_PROTOCOL;

typedef struct {
    uint64_t size, file_size, physical_size;
    uint8_t  create_time[16], last_access_time[16], modification_time[16];
    uint64_t attribute;
    CHAR16   file_name[1];
} EFI_FILE_INFO;

#define EFI_FILE_MODE_READ  1ULL
#define EFI_FILE_DIRECTORY  0x10ULL

/* ---- graphics output ----------------------------------------------------------- */

typedef struct {
    uint32_t version, horizontal_resolution, vertical_resolution, pixel_format;
    uint32_t red_mask, green_mask, blue_mask, reserved_mask;
    uint32_t pixels_per_scan_line;
} EFI_GRAPHICS_OUTPUT_MODE_INFORMATION;

typedef struct {
    uint32_t max_mode, mode;
    EFI_GRAPHICS_OUTPUT_MODE_INFORMATION *info;
    UINTN size_of_info;
    EFI_PHYSICAL_ADDRESS frame_buffer_base;
    UINTN frame_buffer_size;
} EFI_GRAPHICS_OUTPUT_PROTOCOL_MODE;

typedef struct {
    void *query_mode, *set_mode, *blt;
    EFI_GRAPHICS_OUTPUT_PROTOCOL_MODE *mode;
} EFI_GRAPHICS_OUTPUT_PROTOCOL;

enum { PixelRedGreenBlueReserved8BitPerColor, PixelBlueGreenRedReserved8BitPerColor,
       PixelBitMask, PixelBltOnly };

/* ---- GUIDs --------------------------------------------------------------------- */

#define GUID_LOADED_IMAGE   { 0x5B1B31A1, 0x9562, 0x11D2, { 0x8E, 0x3F, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B } }
#define GUID_BLOCK_IO       { 0x964E5B21, 0x6459, 0x11D2, { 0x8E, 0x39, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B } }
#define GUID_SIMPLE_FS      { 0x964E5B22, 0x6459, 0x11D2, { 0x8E, 0x39, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B } }
#define GUID_FILE_INFO      { 0x09576E92, 0x6D3F, 0x11D2, { 0x8E, 0x39, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B } }
#define GUID_GOP            { 0x9042A9DE, 0x23DC, 0x4A38, { 0x96, 0xFB, 0x7A, 0xDE, 0xD0, 0x80, 0x51, 0x6A } }
#define GUID_ACPI20         { 0x8868E871, 0xE4F1, 0x11D3, { 0xBC, 0x22, 0x00, 0x80, 0xC7, 0x3C, 0x88, 0x81 } }
#define GUID_ACPI10         { 0xEB9D2D30, 0x2D88, 0x11D3, { 0x9A, 0x16, 0x00, 0x90, 0x27, 0x3F, 0xC1, 0x4D } }
#define GUID_SMBIOS         { 0xEB9D2D31, 0x2D88, 0x11D3, { 0x9A, 0x16, 0x00, 0x90, 0x27, 0x3F, 0xC1, 0x4D } }
#define GUID_SMBIOS3        { 0xF2FD1544, 0x9794, 0x4A2C, { 0x99, 0x2E, 0xE5, 0xBB, 0xCF, 0x20, 0xE3, 0x94 } }

#endif
