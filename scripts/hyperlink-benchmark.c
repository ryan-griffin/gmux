// Compile against either revision's src directory. Shared-library allocations
// are excluded from --wrap counters; retained bytes describe only link caches.
#include "ghostty_bridge.c"
#include <assert.h>
#include <stdio.h>

static size_t calls, lookups;
GhosttyResult __real_ghostty_grid_ref_hyperlink_uri(const GhosttyGridRef *, uint8_t *, size_t, size_t *);
GhosttyResult __wrap_ghostty_grid_ref_hyperlink_uri(const GhosttyGridRef *ref, uint8_t *buf, size_t len, size_t *written) {
    lookups++;
    return __real_ghostty_grid_ref_hyperlink_uri(ref,buf,len,written);
}
void *__real_malloc(size_t);
void *__real_calloc(size_t,size_t);
void *__real_realloc(void *,size_t);
void *__wrap_malloc(size_t size) { calls++; return __real_malloc(size); }
void *__wrap_calloc(size_t n,size_t size) { calls++; return __real_calloc(n,size); }
void *__wrap_realloc(void *p,size_t size) { calls++; return __real_realloc(p,size); }
static bool discard_cell(void *ud, size_t index, const GmuxCell *cell) {
    (void)ud; (void)index; (void)cell; return true;
}
static void event(void *ud,int kind,const uint8_t *data,size_t len) {
    (void)ud; (void)kind; (void)data; (void)len;
}
static double cpu(void) {
    struct timespec time;
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID,&time);
    return time.tv_sec+time.tv_nsec/1e9;
}
static size_t retained(GmuxEngine *e) {
#ifdef LINK_URI_MAX
    return e->links.spans_cap*sizeof(LinkSpan)+e->links.targets_cap*sizeof(LinkTarget)
        +e->links.bytes_cap+e->held_links.spans_cap*sizeof(LinkSpan)
        +e->held_links.targets_cap*sizeof(LinkTarget)+e->held_links.bytes_cap
        +e->link_scratch.cap*sizeof(uint32_t)
#ifdef GMUX_BENCH_INCREMENTAL
        +e->link_scratch.spare.spans_cap*sizeof(LinkSpan)
        +e->link_scratch.spare.targets_cap*sizeof(LinkTarget)+e->link_scratch.spare.bytes_cap
#endif
        ;
#else
    (void)e; return 0;
#endif
}
int main(int argc,char **argv) {
    const char *workload=argc>1 ? argv[1] : "unlinked-stable";
    int iterations=argc>2 ? atoi(argv[2]) : 1000;
    bool linked=strstr(workload,"unlinked")==NULL;
    bool long_uri=strstr(workload,"long")!=NULL;
    bool output=strstr(workload,"output")!=NULL;
    GmuxEngine *e=gmux_engine_new(NULL,event);
    assert(e);
    GmuxFrame frame={0};  uint8_t flags[24];
    char uri[1600]="test:target/";
    if (long_uri) { memset(uri+12,'x',1500); uri[1512]=0; }
    char initial[4096], rewrite[2048];
    int n=snprintf(initial,sizeof(initial),"\x1b[H%s%s%s",linked ? "\x1b]8;;" : "",linked ? uri : "",linked ? "\x07" : "");
    memset(initial+n,'x',1600); n+=1600;
    if (linked) { memcpy(initial+n,"\x1b]8;;\x07",6); n+=6; }
    gmux_engine_write(e,(uint8_t *)initial,n);
    n=snprintf(rewrite,sizeof(rewrite),"\x1b[H%s%s%s",linked ? "\x1b]8;;" : "",linked ? uri : "",linked ? "\x07" : "");
    memset(rewrite+n,'x',80); n+=80;
    if (linked) { memcpy(rewrite+n,"\x1b]8;;\x07",6); n+=6; }
    for (int i=0;i<5;i++) {
        if (output) gmux_engine_write(e,(uint8_t *)rewrite,n);
        assert(gmux_engine_snapshot(e,&frame,discard_cell,NULL,80*24,flags)>=0);
    }
    calls=lookups=0;
    double start=cpu();
    for (int i=0;i<iterations;i++) {
        if (output) gmux_engine_write(e,(uint8_t *)rewrite,n);
        assert(gmux_engine_snapshot(e,&frame,discard_cell,NULL,80*24,flags)>=0);
    }
    double duration=cpu()-start;
    size_t before_clear=retained(e), allocations=calls, uri_lookups=lookups;
    gmux_engine_write(e,(uint8_t *)"\x1b[H\x1b[2Jplain",12);
    assert(gmux_engine_snapshot(e,&frame,discard_cell,NULL,80*24,flags)>=0);
    printf("{\"workload\":\"%s\",\"iterations\":%d,\"cpu_ns_per_snapshot\":%.1f,\"bridge_allocations\":%zu,\"uri_lookups\":%zu,\"link_retained_bytes\":%zu,\"post_clear_bytes\":%zu,\"engine_bytes\":%zu,\"view_bytes\":%zu,\"cell_bytes\":%zu,\"frame_bytes\":%zu}\n",
        workload,iterations,duration*1e9/iterations,allocations,uri_lookups,before_clear,retained(e),sizeof(*e),sizeof(GmuxView),sizeof(GmuxCell),sizeof(GmuxFrame));
    gmux_engine_free(e);
}
