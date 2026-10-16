#!/usr/bin/env python3
"""Freestanding ARM/WinCE GPS1 diagnostic; no proprietary SDK sources."""
import argparse, importlib.util, os, zipfile
from pathlib import Path
ROOT = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('pe_builder', ROOT.parent/'ramtest/build.py')
b = importlib.util.module_from_spec(spec); spec.loader.exec_module(b)
NAMES = ['CreateFileW','ReadFile','WriteFile','CloseHandle','GetLastError','MessageBoxW','GetTickCount','Sleep']
SIG = {**b.SIGNATURES, 'GetTickCount':('unsigned int','void'),
       'DeviceIoControl':('unsigned int','unsigned int,unsigned int,void*,unsigned int,void*,unsigned int,unsigned int*,void*')}
def main():
    p = argparse.ArgumentParser()
    p.add_argument('--clang', default=os.environ.get('CLANG','clang'))
    p.add_argument('--lld', default=os.environ.get('LLD','ld.lld'))
    p.add_argument('--work', type=Path, default=ROOT/'build')
    args = p.parse_args(); args.work.mkdir(parents=True, exist_ok=True)
    header = '/* Generated WinCE IAT bindings. */\n'
    for i, name in enumerate(NAMES):
        ret, params = SIG[name]; header += f'#define {name} (*({ret} (**)({params}))0x{0x30100+i*4:08x}u)\n'
    (args.work/'imports.h').write_text(header)
    code, symbols = b.compile_image(args,'gpstest',0x11000,ROOT/'src/gpstest.c')
    assert len(code)<0x1f000
    out = ROOT/'dist/GZGP999994'; out.mkdir(parents=True,exist_ok=True)
    b.pe(out/'AUTORUN.EXE',0x10000,symbols['entry']-0x10000,
         [('.image',0x1000,b.align(len(code),4096),code,0xe0000060),('.idata',0x20000,4096,b.imports(NAMES),0xc0000040)],
         {1:(0x20000,40),12:(0x20100,4*(len(NAMES)+1))})
    (out/'GZGP999994').write_bytes(b.U32(999994))
    with zipfile.ZipFile(ROOT/'dist/PocketHLE-GPSTEST.zip','w',zipfile.ZIP_DEFLATED) as z:
        for file in sorted(out.iterdir()): z.write(file,'GZGP999994/'+file.name)
    print(ROOT/'dist/PocketHLE-GPSTEST.zip')
if __name__=='__main__': main()
