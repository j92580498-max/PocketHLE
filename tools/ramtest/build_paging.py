"""A DLL with independent cold code, initialized data and zero-fill pages."""
import struct
from build import ROOT, U16, U32, pe

def build(args):
    code=bytearray(8192)
    code[:8]=U32(0xe3a00001)+U32(0xe12fff1e)  # DllMain returns TRUE
    code[4096:4104]=U32(0xe3a0004d)+U32(0xe12fff1e)  # separately paged export
    code[0x800:0x828]=struct.pack('<IIHHIIIIIII',0,0,0,0,0x1860,1,2,2,0x1828,0x1830,0x1838)
    for index,(name,rva) in enumerate([('FarCode',0x2000),('PageData',0x3000)]):
        code[0x828+index*4:0x82c+index*4]=U32(rva)
        code[0x830+index*4:0x834+index*4]=U32(0x1880+index*16)
        code[0x838+index*2:0x83a+index*2]=U16(index)
        raw=name.encode()+b'\0';code[0x880+index*16:0x880+index*16+len(raw)]=raw
    code[0x860:0x86e]=b'pageprobe.dll\0'
    data=bytearray(12288);data[:4]=U32(0x1234ABCD);data[8192:8196]=U32(0xDEAD1234)
    relocs=U32(0x1000)+U32(12)+U16(0)+U16(0)
    pe(ROOT/'dist/GZRT999999/pageprobe.dll',0x30000000,0x1000,
       [('.text',0x1000,8192,bytes(code),0x60000020),('.data',0x3000,0x20000,bytes(data),0xc0000040),
        ('.reloc',0x23000,4096,relocs,0x42000040)],{0:(0x1800,0x100),5:(0x23000,12)},True)
