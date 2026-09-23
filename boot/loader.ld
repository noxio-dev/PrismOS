OUTPUT_FORMAT(binary)
OUTPUT_ARCH(i386)

SECTIONS
{
    . = 0x7E00;

    .text : {
        *(.text)
        *(.rodata)
        *(.data)
    }

    /DISCARD/ : {
        *(.bss)
        *(COMMON)
        *(.note*)
        *(.comment*)
    }
}
