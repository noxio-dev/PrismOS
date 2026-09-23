OUTPUT_FORMAT(binary)
OUTPUT_ARCH(i386)
ENTRY(kstart)

SECTIONS
{
    . = 0x20000;

    .text : {
        *(.text*)
    }
    .rodata : {
        *(.rodata*)
    }
    .data : {
        *(.data*)
    }

    /DISCARD/ : {
        *(.bss) *(COMMON) *(.note*) *(.comment*) *(.eh_frame)
    }
}
