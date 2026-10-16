"""Native-import PE fixtures, with position-independent IAT call veneers."""
from pathlib import Path
import struct
from build import ROOT, U16, U32, align, pe, compile_image

def bindings(items):
    data=bytearray(4096);groups={}
    for slot,(dll,name) in enumerate(items):groups.setdefault(dll,[]).append((slot,name))
    names=0x600;lookup=0x400
    for descriptor,(dll,entries) in enumerate(groups.items()):
        raw=dll.encode()+b'\0';data[names:names+len(raw)]=raw;dllrva=0x24000+names;names=align(names+len(raw),2)
        offset=descriptor*20
        data[offset:offset+20]=struct.pack('<IIIII',0x24000+lookup,0,0,dllrva,0x24100+(entries[0][0]+descriptor)*4)
        for slot,name in entries:
            if isinstance(name,int):value=0x80000000|name
            else:
                raw=U16(0)+name.encode()+b'\0';data[names:names+len(raw)]=raw;value=0x24000+names;names=align(names+len(raw),2)
            data[lookup:lookup+4]=U32(value);lookup+=4
            data[0x100+(slot+descriptor)*4:0x104+(slot+descriptor)*4]=U32(value)
        lookup+=4
    return bytes(data),(len(groups)+1)*20

def build(args):
    out=ROOT/'dist'/'GZRT999999'
    system=[('coredll.dll','CreateFileW'),('coredll.dll','SetFilePointer'),('coredll.dll','WriteFile'),('coredll.dll','CloseHandle')]
    configs=[('depleaf',0,[]),('deproot',1,[('depleaf.dll','Value')]),
        ('deppeer',2,[('depleaf.dll',2)]),('depreject',3,[('depleaf.dll','Value')]),
        ('depmissing',1,[('does-not-exist.dll','Value')]),('depbadexport',1,[('depleaf.dll','Missing')]),
        ('depcyclea',4,[('depcycleb.dll','Value')]),('depcycleb',5,[('depcyclea.dll','Helper')])]
    for name,kind,native in configs:
        items=native+system
        funcs=(['dep_call'] if native else [])+['CreateFileW','SetFilePointer','WriteFile','CloseHandle']
        asm=['.syntax unified','.arm','.text']
        # The literal holds IAT-PC, so rebasing the whole DLL needs no relocation.
        for index,func in enumerate(funcs):
            asm.extend([f'.global {func}',f'.hidden {func}',f'{func}:',f'ldr ip, {func}_offset',
                f'{func}_add:', 'add ip, pc, ip','ldr ip, [ip]','bx ip',f'{func}_offset:',f'.word 0x30024100+{(index+int(bool(native) and index>0))*4}-({func}_add+8)'])
        source=args.work/(name+'.c')
        assembly='\n'.join(asm)+'\n'
        source.write_text('#define KIND '+str(kind)+'\n'+(ROOT/'src/dep.c').read_text()+'\n__asm__('+repr(assembly).replace("'",'"')+');\n')
        code,syms=compile_image(args,name,0x30001000,source,('-fPIC',));assert len(code)<0x800
        # Resolve PC-relative literals explicitly in the generated PE payload.
        # The ELF absolute expression otherwise retains an ARM relocation addend.
        code=bytearray(code)
        for index,func in enumerate(funcs):
            slot=index+int(bool(native) and index>0)
            offset=syms[func+'_offset']-0x30001000
            code[offset:offset+4]=U32((0x30024100+slot*4-(syms[func+'_add']+8)) & 0xffffffff)
        data=bytearray(4096);data[:len(code)]=code
        exports=['Helper','Value']
        data[0x800:0x828]=struct.pack('<IIHHIIIIIII',0,0,0,0,0x1860,1,2,2,0x1828,0x1830,0x1838)
        for index,symbol in enumerate(exports):
            data[0x828+index*4:0x82c+index*4]=U32(syms[symbol]-0x30000000)
            data[0x830+index*4:0x834+index*4]=U32(0x1880+index*16)
            data[0x838+index*2:0x83a+index*2]=U16(index)
            raw=symbol.encode()+b'\0';data[0x880+index*16:0x880+index*16+len(raw)]=raw
        raw=(name+'.dll').encode()+b'\0';data[0x860:0x860+len(raw)]=raw
        idata,size=bindings(items);relocs=U32(0x1000)+U32(12)+U16(0)+U16(0)
        pe(out/(name+'.dll'),0x30000000,syms['entry']-0x30000000,
            [('.text',0x1000,4096,bytes(data),0xe0000060),('.data',0x2000,0x20000,b'',0xc0000040),
             ('.reloc',0x22000,4096,relocs,0x42000040),('.idata',0x24000,4096,idata,0xc0000040)],
            {0:(0x1800,0x100),1:(0x24000,size),5:(0x22000,12),12:(0x24100,4*(len(items)+len(set(dll for dll,_ in items))))},True)
