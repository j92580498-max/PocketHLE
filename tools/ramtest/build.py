#!/usr/bin/env python3
"""Build freestanding ARM C with Clang/LLD, then wrap it as WinCE PE32.
Requires Python 3, clang and ld.lld; no Windows SDK or proprietary library.
"""
import argparse,json,os,struct,subprocess,zipfile
from pathlib import Path
ROOT=Path(__file__).resolve().parent
U16=lambda x:struct.pack('<H',x)
U32=lambda x:struct.pack('<I',x)
def align(n,a):return (n+a-1)&~(a-1)
SIGNATURES={
 'CreateMutexW':('unsigned int','void*,unsigned int,const unsigned short*'),
 'ReleaseMutex':('unsigned int','unsigned int'),
 'CreateProcessW':('unsigned int','const unsigned short*,const unsigned short*,void*,void*,unsigned int,unsigned int,void*,void*,void*,void*'),
 'SuspendThread':('unsigned int','unsigned int'), 'TerminateProcess':('unsigned int','unsigned int,unsigned int'),
 'GetExitCodeProcess':('unsigned int','unsigned int,unsigned int*'),
 'GetCurrentProcess':('unsigned int','void'), 'GetCurrentProcessId':('unsigned int','void'),
 'GetCommandLineW':('const unsigned short*','void'),
 'OpenProcess':('unsigned int','unsigned int,unsigned int,unsigned int'),
 'DuplicateHandle':('unsigned int','unsigned int,unsigned int,unsigned int,unsigned int*,unsigned int,unsigned int,unsigned int'),
 'ResetEvent':('unsigned int','unsigned int'),
 'TlsAlloc':('unsigned int','void'), 'TlsFree':('unsigned int','unsigned int'),
 'TlsGetValue':('unsigned int','unsigned int'), 'TlsSetValue':('unsigned int','unsigned int,unsigned int'),
 'WaitForMultipleObjects':('unsigned int','unsigned int,const unsigned int*,unsigned int,unsigned int'),
 'GetSystemMemoryDivision':('unsigned int','unsigned int*,unsigned int*,unsigned int*'),
 'SetSystemMemoryDivision':('unsigned int','unsigned int'),
 'GlobalMemoryStatus':('void','void*'),
 'VirtualAlloc':('void*','void*,unsigned int,unsigned int,unsigned int'),
 'VirtualFree':('unsigned int','void*,unsigned int,unsigned int'),
 'LocalAlloc':('void*','unsigned int,unsigned int'), 'LocalFree':('unsigned int','void*'),
 'LocalSize':('unsigned int','void*'), 'LocalReAlloc':('void*','void*,unsigned int,unsigned int'),
 'HeapFree':('unsigned int','unsigned int,unsigned int,void*'), 'HeapSize':('unsigned int','unsigned int,unsigned int,void*'),
 'HeapReAlloc':('void*','unsigned int,unsigned int,void*,unsigned int'), 'GetProcessHeap':('unsigned int','void'),
 'CreateThread':('unsigned int','void*,unsigned int,unsigned int(*)(void*),void*,unsigned int,unsigned int*'),
 'CreateEventW':('unsigned int','void*,unsigned int,unsigned int,const unsigned short*'), 'SetEvent':('unsigned int','unsigned int'), 'Sleep':('void','unsigned int'),
 'ExitProcess':('void','unsigned int'),
 'GetCurrentThreadId':('unsigned int','void'), 'ExitThread':('void','unsigned int'), 'TerminateThread':('unsigned int','unsigned int,unsigned int'),
 'ResumeThread':('unsigned int','unsigned int'),
 'WaitForSingleObject':('unsigned int','unsigned int,unsigned int'),
 'GetExitCodeThread':('unsigned int','unsigned int,unsigned int*'),
 'CloseHandle':('unsigned int','unsigned int'),
 'LoadLibraryW':('unsigned int','const unsigned short*'),
 'FreeLibrary':('unsigned int','unsigned int'),
 'GetProcAddressA':('void*','unsigned int,const char*'),
 'SetLastError':('void','unsigned int'), 'GetLastError':('unsigned int','void'),
 'GetModuleFileNameW':('unsigned int','unsigned int,unsigned short*,unsigned int'),
 'CreateFileW':('unsigned int','const unsigned short*,unsigned int,unsigned int,void*,unsigned int,unsigned int,unsigned int'),
 'ReadFile':('unsigned int','unsigned int,void*,unsigned int,unsigned int*,void*'),
 'SetFilePointer':('unsigned int','unsigned int,unsigned int,unsigned int*,unsigned int'),
 'WriteFile':('unsigned int','unsigned int,const void*,unsigned int,unsigned int*,void*'),
 'MessageBoxW':('unsigned int','unsigned int,const unsigned short*,const unsigned short*,unsigned int')}
def imports(names):
 data=bytearray(4096);rva=0x20000
 data[:20]=struct.pack('<IIIII',rva+0x400,0,0,rva+0x80,rva+0x100)
 data[0x80:0x8c]=b'coredll.dll\0'
 cursor=0x600
 for i,name in enumerate(names):
  raw=U16(0)+name.encode()+b'\0';data[cursor:cursor+len(raw)]=raw
  data[0x400+i*4:0x404+i*4]=U32(rva+cursor);data[0x100+i*4:0x104+i*4]=U32(rva+cursor)
  cursor=align(cursor+len(raw),2)
 return bytes(data)
def pe(path,base,entry,sections,directories,dll=False):
 headers=bytearray(0x400);headers[:2]=b'MZ';headers[0x3c:0x40]=U32(0x80);headers[0x80:0x84]=b'PE\0\0'
 headers[0x84:0x98]=struct.pack('<HHIIIHH',0x1c0,len(sections),0,0,0,224,0x102|(0x2000 if dll else 0))
 opt=0x98;headers[opt:opt+2]=U16(0x10b)
 size_image=align(max(rva+size for _,rva,size,_,_ in sections),4096)
 for off,val in [(16,entry),(20,0x1000),(28,base),(32,4096),(36,512),(56,size_image),(60,len(headers)),(72,0x10000),(76,4096),(80,0x10000),(84,4096),(92,16)]:headers[opt+off:opt+off+4]=U32(val)
 headers[opt+68:opt+70]=U16(9)
 for index,(rva,size) in directories.items():headers[opt+96+index*8:opt+104+index*8]=U32(rva)+U32(size)
 body=bytearray();offset=len(headers)
 for index,(name,rva,size,data,flags) in enumerate(sections):
  raw_size=align(len(data),512) if data else 0;h=0x178+index*40
  headers[h:h+40]=name.encode().ljust(8,b'\0')+struct.pack('<IIIIIIHHI',size,rva,raw_size,offset if data else 0,0,0,0,0,flags)
  body.extend(data);body.extend(b'\0'*(raw_size-len(data)));offset+=raw_size
 path.write_bytes(headers+body)
def symbols(path):
 b=path.read_bytes();off=struct.unpack_from('<I',b,32)[0];ent,count=struct.unpack_from('<HH',b,46);sections=[struct.unpack_from('<IIIIIIIIII',b,off+i*ent) for i in range(count)];result={}
 for s in sections:
  if s[1]!=2:continue
  strings=sections[s[6]];names=b[strings[4]:strings[4]+strings[5]]
  for pos in range(s[4],s[4]+s[5],s[9]):
   name,val,_,_,_,_=struct.unpack_from('<IIIBBH',b,pos);end=names.find(b'\0',name);result[names[name:end].decode()]=val
 return result
def compile_image(args,name,base,source,defines=()):
 obj=args.work/(name+'.o');elf=args.work/(name+'.elf');binary=args.work/(name+'.bin');script=args.work/(name+'.ld')
 script.write_text(f'ENTRY(entry)\nSECTIONS {{ . = 0x{base:x}; .image : {{ __image_start = .; *(.text.entry) *(.text*) *(.rodata*) *(.data*) *(.bss*) *(COMMON) __image_end = .; }} /DISCARD/ : {{ *(.ARM.exidx*) *(.ARM.extab*) *(.comment*) *(.note*) }} }}\n')
 subprocess.run([args.clang,'--target=armv4t-none-eabi','-marm','-mfloat-abi=soft','-ffreestanding','-fno-builtin','-fshort-wchar','-fno-stack-protector','-fno-unwind-tables','-fno-asynchronous-unwind-tables','-O1','-I',str(args.work),'-c',str(source),'-o',str(obj),*defines],check=True)
 common=[args.lld,'-m','armelf','-T',str(script),str(obj)]
 subprocess.run([*common,'-o',str(elf)],check=True);subprocess.run([*common,'--oformat=binary','-o',str(binary)],check=True)
 return binary.read_bytes(),symbols(elf)
def main():
 p=argparse.ArgumentParser();p.add_argument('--clang',default=os.environ.get('CLANG','clang'));p.add_argument('--lld',default=os.environ.get('LLD','ld.lld'));p.add_argument('--work',type=Path,default=ROOT/'build');args=p.parse_args();args.work.mkdir(parents=True,exist_ok=True)
 names=json.loads((ROOT/'src/imports.json').read_text());header='/* Generated IAT bindings for this WinCE PE. */\n'
 for i,name in enumerate(names):
  ret,params=SIGNATURES[name];header+=f'#define {name} (*({ret} (**)({params}))0x{0x30100+i*4:08x}u)\n'
 (args.work/'imports.h').write_text(header)
 out=ROOT/'dist'/'GZRT999999';out.mkdir(parents=True,exist_ok=True)
 code,syms=compile_image(args,'ramtest',0x11000,ROOT/'src/ramtest.c');assert len(code)<0x1f000
 pe(out/'AUTORUN.EXE',0x10000,syms['entry']-0x10000,[('.image',0x1000,align(len(code),4096),code,0xe0000060),('.idata',0x20000,4096,imports(names),0xc0000040)],{1:(0x20000,40),12:(0x20100,4*(len(names)+1))})
 fixtures=ROOT/'dist'/'fixtures';fixtures.mkdir(exist_ok=True)
 for variant,define in [('explicit-exit','-DTEST_EXIT_PROCESS'),('last-worker-exit','-DTEST_LAST_WORKER_EXIT')]:
  code,syms=compile_image(args,variant,0x11000,ROOT/'src/ramtest.c',(define,))
  pe(fixtures/(variant+'.exe'),0x10000,syms['entry']-0x10000,[('.image',0x1000,align(len(code),4096),code,0xe0000060),('.idata',0x20000,4096,imports(names),0xc0000040)],{1:(0x20000,40),12:(0x20100,4*(len(names)+1))})

 for proc in ['proctest','procworker']:
  code,syms=compile_image(args,proc,0x11000,ROOT/('src/'+proc+'.c'));assert len(code)<0x1f000
  pe(out/(proc+'.exe'),0x10000,syms['entry']-0x10000,[('.image',0x1000,align(len(code),4096),code,0xe0000060),('.idata',0x20000,4096,imports(names),0xc0000040)],{1:(0x20000,40),12:(0x20100,4*(len(names)+1))})
 (out/'badproc.exe').write_bytes(b'not a PE image')

 for name,defines in [('ramprobe',('-fPIC',)),('ramprobe2',('-fPIC',)),('ramreject',('-fPIC','-DREJECT_ATTACH'))]:
  code,syms=compile_image(args,name,0x30001000,ROOT/'src/ramprobe.c',defines);assert len(code)<0x800
  data=bytearray(4096);data[:len(code)]=code;exp=0x1800
  data[0x800:0x828]=struct.pack('<IIHHIIIIIII',0,0,0,0,0x1860,1,2,2,0x1828,0x1830,0x1838)
  data[0x828:0x830]=U32(syms['Configure']-0x30000000)+U32(syms['RamProbe']-0x30000000)
  data[0x830:0x838]=U32(0x1880)+U32(0x1890);data[0x838:0x83c]=U16(0)+U16(1)
  dll_name=(name+'.dll').encode()+b'\0';data[0x860:0x860+len(dll_name)]=dll_name
  data[0x880:0x88a]=b'Configure\0';data[0x890:0x899]=b'RamProbe\0'
  relocs=U32(0x1000)+U32(12)+U16(0)+U16(0)
  pe(out/(name+'.dll'),0x30000000,syms['entry']-0x30000000,[('.text',0x1000,4096,bytes(data),0xe0000060),('.data',0x2000,0x20000,b'',0xc0000040),('.reloc',0x22000,4096,relocs,0x42000040)],{0:(exp,0x80),5:(0x22000,12)},True)
 # Synthetic diagnostic title marker: enables the existing Gizmondo profile.
 from build_dependencies import build as build_dependencies
 build_dependencies(args)
 from build_paging import build as build_paging
 build_paging(args)
 (out/'GZRT999999').write_bytes(U32(999999))
 with zipfile.ZipFile(ROOT/'dist'/'PocketHLE-RAMTEST.zip','w',zipfile.ZIP_DEFLATED) as z:
  for path in sorted(out.iterdir()):z.write(path,'GZRT999999/'+path.name)
 print(ROOT/'dist'/'PocketHLE-RAMTEST.zip')
if __name__=='__main__':main()
