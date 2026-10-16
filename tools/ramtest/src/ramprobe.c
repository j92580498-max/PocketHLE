typedef unsigned int U32;
static void (*notification)(U32,U32,U32);
__attribute__((section(".text.entry"))) U32 entry(U32 module,U32 reason,void *reserved) {
#ifdef REJECT_ATTACH
    return reason!=1;
#else
    if(notification) notification(module,reason,(U32)reserved);
    // Only PROCESS_ATTACH consumes the return value. Other reasons deliberately fail.
    return reason==1;
#endif
}
void Configure(void (*callback)(U32,U32,U32)) { notification=callback; }
U32 RamProbe(void) { return 77; }
