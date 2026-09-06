/* Linker script: lay the image out at its final Windows CE VAs. */
ENTRY(WinMain)

SECTIONS
{
    . = 0x00013000;

    .text : {
        *(.text*)
        *(.rodata*)
        *(.glue_7)
        *(.glue_7t)
        *(.vfp11_veneer)
        *(.iplt)
        *(.igot.plt)
    }

    . = ALIGN(0x1000);
    .idata : {
        KEEP(*(.idata))
    }

    . = ALIGN(0x1000);
    .data : {
        *(.data*)
        *(.sdata*)
    }

    . = ALIGN(0x1000);
    .bss : {
        *(.bss*)
        *(.sbss*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.ARM.exidx*)
        *(.ARM.attributes)
        *(.comment)
        *(.note*)
    }
}
