#!/usr/bin/env python3
"""Build a freestanding ARM CE Bluetooth inquiry diagnostic, without SDK code."""
import argparse, importlib.util, os, struct, zipfile
from pathlib import Path
ROOT=Path(__file__).resolve().parent
BUILDER=ROOT.parent/'ramtest/build.py'
if not BUILDER.exists():BUILDER=ROOT/'pe_builder.py'
spec=importlib.util.spec_from_file_location('pe_builder',BUILDER)
b=importlib.util.module_from_spec(spec);spec.loader.exec_module(b)
DLLS={
 'coredll.dll':['CreateFileW','WriteFile','CloseHandle','GetLastError','VirtualAlloc','VirtualFree','MessageBoxW','RegisterWindowMessageW','SendMessageW','Sleep'],
 'ws2.dll':['WSAStartup','WSACleanup','WSAGetLastError','gethostname','WSALookupServiceBeginW','WSALookupServiceNextW','WSALookupServiceEnd']}
SIG={**b.SIGNATURES,
 'RegisterWindowMessageW':('unsigned int','const unsigned short*'),
 'SendMessageW':('unsigned int','unsigned int,unsigned int,unsigned int,unsigned int'),
 'WSAStartup':('unsigned int','unsigned int,void*'),
 'WSACleanup':('unsigned int','void'), 'WSAGetLastError':('unsigned int','void'),
 'gethostname':('unsigned int','char*,unsigned int'),
 'WSALookupServiceBeginW':('unsigned int','void*,unsigned int,unsigned int*'),
 'WSALookupServiceNextW':('unsigned int','unsigned int,unsigned int,unsigned int*,void*'),
 'WSALookupServiceEnd':('unsigned int','unsigned int')}
def imports():
    data=bytearray(4096);cursor=0x600
    for group,(dll,names) in enumerate(DLLS.items()):
        iat=0x100+group*0x100;ilt=0x400+group*0x100;name=0x80+group*0x20
        data[group*20:group*20+20]=struct.pack('<IIIII',0x20000+ilt,0,0,0x20000+name,0x20000+iat)
        raw=dll.encode()+b'\0';data[name:name+len(raw)]=raw
        for i,api in enumerate(names):
            raw=b.U16(0)+api.encode()+b'\0';data[cursor:cursor+len(raw)]=raw
            data[iat+i*4:iat+i*4+4]=b.U32(0x20000+cursor)
            data[ilt+i*4:ilt+i*4+4]=b.U32(0x20000+cursor)
            cursor=b.align(cursor+len(raw),2)
    assert cursor<4096
    return bytes(data)
def main():
    p=argparse.ArgumentParser();p.add_argument('--clang',default=os.environ.get('CLANG','clang'));p.add_argument('--lld',default=os.environ.get('LLD','ld.lld'))
    p.add_argument('--work',type=Path,default=ROOT/'build');a=p.parse_args();a.work.mkdir(parents=True,exist_ok=True)
    header='/* Generated WinCE IAT bindings for COREDLL and WS2. */\n'
    for group,names in enumerate(DLLS.values()):
        for i,api in enumerate(names):
            ret,params=SIG[api];header+=f'#define {api} (*({ret} (**)({params}))0x{0x30100+group*0x100+i*4:08x}u)\n'
    (a.work/'imports.h').write_text(header)
    code,syms=b.compile_image(a,'bttest',0x11000,ROOT/'src/bttest.c');assert len(code)<0x1f000
    out=ROOT/'dist/GZBT999996';out.mkdir(parents=True,exist_ok=True)
    b.pe(out/'AUTORUN.EXE',0x10000,syms['entry']-0x10000,
         [('.image',0x1000,b.align(len(code),4096),code,0xe0000060),('.idata',0x20000,4096,imports(),0xc0000040)],
         {1:(0x20000,60),12:(0x20100,0x100+4*(len(DLLS['ws2.dll'])+1))})
    (out/'GZBT999996').write_bytes(b.U32(999996))
    source=out/'source';(source/'src').mkdir(parents=True,exist_ok=True)
    (source/'build.py').write_bytes(Path(__file__).read_bytes())
    (source/'pe_builder.py').write_bytes(BUILDER.read_bytes())
    (source/'src/bttest.c').write_bytes((ROOT/'src/bttest.c').read_bytes())
    (out/'README-BTTEST.md').write_bytes((ROOT/'README.md').read_bytes())
    with zipfile.ZipFile(ROOT/'PocketHLE-BTTEST-ARM.zip','w',zipfile.ZIP_DEFLATED) as z:
        for file in sorted(out.rglob('*')):
            if file.is_file():z.write(file,'GZBT999996/'+file.relative_to(out).as_posix())
    print(ROOT/'PocketHLE-BTTEST-ARM.zip')
if __name__=='__main__':main()
