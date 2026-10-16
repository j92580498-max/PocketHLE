#!/usr/bin/env python3
"""Build the complete freestanding ARM VFS diagnostic without a proprietary SDK."""
import argparse,importlib.util,json,os,sys,zipfile
from pathlib import Path
ROOT=Path(__file__).resolve().parent
RAM=ROOT.parent/'ramtest';sys.path.insert(0,str(RAM))
spec=importlib.util.spec_from_file_location('pe_builder',RAM/'build.py');b=importlib.util.module_from_spec(spec);spec.loader.exec_module(b)
ADDITIONAL={
'DeleteFileW':('unsigned int','const unsigned short*'),
'MoveFileW':('unsigned int','const unsigned short*,const unsigned short*'),
'CreateDirectoryW':('unsigned int','const unsigned short*,void*'),
'RemoveDirectoryW':('unsigned int','const unsigned short*'),
'GetFileAttributesW':('unsigned int','const unsigned short*'),
'SetFileAttributesW':('unsigned int','const unsigned short*,unsigned int'),
'SetEndOfFile':('unsigned int','unsigned int'),
'GetFileSize':('unsigned int','unsigned int,unsigned int*'),
'FlushFileBuffers':('unsigned int','unsigned int'),
'GetDiskFreeSpaceExW':('unsigned int','const unsigned short*,void*,void*,void*'),
'FindFirstFileW':('unsigned int','const unsigned short*,void*'),
'FindNextFileW':('unsigned int','unsigned int,void*'),
'FindClose':('unsigned int','unsigned int'),
'fopen':('unsigned int','const char*,const char*'),
'fclose':('unsigned int','unsigned int'),
'fwrite':('unsigned int','const void*,unsigned int,unsigned int,unsigned int'),
'fread':('unsigned int','void*,unsigned int,unsigned int,unsigned int'),
'fseek':('unsigned int','unsigned int,int,int'),
'ftell':('unsigned int','unsigned int')}
def main():
 p=argparse.ArgumentParser();p.add_argument('--clang',default=os.environ.get('CLANG','clang'));p.add_argument('--lld',default=os.environ.get('LLD','ld.lld'));p.add_argument('--work',type=Path,default=ROOT/'build');args=p.parse_args();args.work.mkdir(parents=True,exist_ok=True)
 names=json.loads((ROOT/'src/imports.json').read_text());signatures={**b.SIGNATURES,**ADDITIONAL}
 header='/* Generated ARM WinCE IAT bindings. */\n'
 for i,name in enumerate(names):
  ret,params=signatures[name];header+=f'#define {name} (*({ret} (**)({params}))0x{0x30100+i*4:08x}u)\n'
 (args.work/'imports.h').write_text(header)
 out=ROOT/'dist/GZVT999998';out.mkdir(parents=True,exist_ok=True)
 for name,defs in [('AUTORUN.EXE',()),('vfsworker.exe',('-DWORKER',))]:
  code,syms=b.compile_image(args,name,0x11000,ROOT/'src/vfstest.c',defs);assert len(code)<0x1f000
  b.pe(out/name,0x10000,syms['entry']-0x10000,[('.image',0x1000,b.align(len(code),4096),code,0xe0000060),('.idata',0x20000,4096,b.imports(names),0xc0000040)],{1:(0x20000,40),12:(0x20100,4*(len(names)+1))})
 (out/'GZVT999998').write_bytes(b.U32(999998));(out/'asset.bin').write_bytes(b'CARD-DATA')
 with zipfile.ZipFile(ROOT/'dist/PocketHLE-VFSTEST.zip','w',zipfile.ZIP_DEFLATED) as z:
  for p in sorted(out.iterdir()):z.write(p,'GZVT999998/'+p.name)
 print(ROOT/'dist/PocketHLE-VFSTEST.zip')
if __name__=='__main__':main()
