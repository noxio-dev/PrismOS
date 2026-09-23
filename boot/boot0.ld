OUTPUT_FORMAT(binary)
SECTIONS
{
    . = 0x7C00;
    .text   : { *(.text*)   }
    .rodata : { *(.rodata*) }
    . = 0x7C00 + 510;
    .sig    : { *(.sig)     }
}