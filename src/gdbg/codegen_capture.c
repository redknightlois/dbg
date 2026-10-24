// CUPTI injection library: CUDA loads it from CUDA_INJECTION64_PATH and calls InitializeInjection.
// Writes every loaded module's cubin and the kernel symbols launched through the runtime or driver
// API into GDBG_CAPTURE_DIR as module-<pid>-<seq>.cubin and records-<pid>.jsonl.
#define _GNU_SOURCE
#include <cuda.h>
#include <cupti.h>
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

// Fixed-size symbol table; launches past it are counted as dropped.
#define SLOTS 65536

// Recursive: driver calls made while recording a module re-enter the callback.
static pthread_mutex_t lock = PTHREAD_RECURSIVE_MUTEX_INITIALIZER_NP;
static FILE *records;
static const char *dir;
static unsigned module_seq;
static unsigned long long graph_launches, dropped;
static struct {
    char *symbol;
    unsigned long long count;
} launches[SLOTS];

static void json_string(const char *s) {
    if (s == NULL) {
        fputs("null", records);
        return;
    }
    fputc('"', records);
    for (; *s; ++s) {
        unsigned char c = (unsigned char)*s;
        if (c == '"' || c == '\\')
            fprintf(records, "\\%c", c);
        else if (c < 0x20)
            fprintf(records, "\\u%04x", c);
        else
            fputc(c, records);
    }
    fputc('"', records);
}

static void record_error(const char *call, CUptiResult result) {
    const char *message = NULL;
    cuptiGetResultString(result, &message);
    fprintf(records, "{\"event\":\"error\",\"pid\":%d,\"call\":\"%s\",\"result\":%d,\"message\":", getpid(), call, (int)result);
    json_string(message);
    fputs("}\n", records);
    fflush(records);
}

static void on_module(const CUpti_ResourceData *resource) {
    const CUpti_ModuleResourceData *module = (const CUpti_ModuleResourceData *)resource->resourceDescriptor;
    if (module == NULL || module->pCubin == NULL)
        return;
    char name[64], path[4096];
    snprintf(name, sizeof name, "module-%d-%u.cubin", getpid(), module_seq++);
    snprintf(path, sizeof path, "%s/%s", dir, name);
    FILE *out = fopen(path, "wb");
    size_t written = out ? fwrite(module->pCubin, 1, module->cubinSize, out) : 0;
    if (out)
        fclose(out);

    uint32_t device_id = 0;
    int major = -1, minor = -1;
    CUdevice device;
    if (cuptiGetDeviceId(resource->context, &device_id) == CUPTI_SUCCESS && cuDeviceGet(&device, (int)device_id) == CUDA_SUCCESS) {
        cuDeviceGetAttribute(&major, CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR, device);
        cuDeviceGetAttribute(&minor, CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR, device);
    }
    Dl_info origin;
    int has_origin = dladdr(module->pCubin, &origin) && origin.dli_fname != NULL;

    fprintf(records, "{\"event\":\"module\",\"pid\":%d,\"module_id\":%u,\"file\":\"%s\",\"size\":%zu,\"written\":%zu,\"device\":%u,\"device_cc\":[%d,%d],\"image\":",
            getpid(), module->moduleId, name, module->cubinSize, written, device_id, major, minor);
    json_string(has_origin ? origin.dli_fname : NULL);
    fputs("}\n", records);
    fflush(records);
}

// Runtime API calls re-enter through the driver API; only the outermost call is counted.
static __thread int runtime_depth;

static void count_launch(CUpti_CallbackDomain domain, CUpti_CallbackId id, const CUpti_CallbackData *call) {
    const char *fn = call->functionName;
    if (strncmp(fn, "cuGraphLaunch", 13) == 0 || strncmp(fn, "cudaGraphLaunch", 15) == 0) {
        ++graph_launches;
        return;
    }
    if ((strncmp(fn, "cuLaunch", 8) != 0 && strncmp(fn, "cudaLaunch", 10) != 0) || strstr(fn, "HostFunc") != NULL)
        return;
    // symbolName is valid only for launch callbacks.
    const char *symbol = call->symbolName;
#if CUDA_VERSION >= 12030
    // CUPTI leaves symbolName empty for cuLaunchKernelEx (Triton's launch path).
    if ((symbol == NULL || *symbol == '\0') && domain == CUPTI_CB_DOMAIN_DRIVER_API &&
        (id == CUPTI_DRIVER_TRACE_CBID_cuLaunchKernelEx || id == CUPTI_DRIVER_TRACE_CBID_cuLaunchKernelEx_ptsz))
        cuFuncGetName(&symbol, ((const cuLaunchKernelEx_params *)call->functionParams)->f);
#else
    (void)domain;
    (void)id;
#endif
    if (symbol == NULL)
        symbol = "";
    // FNV-1a, linear probing.
    unsigned long long hash = 1469598103934665603ULL;
    for (const char *p = symbol; *p; ++p)
        hash = (hash ^ (unsigned char)*p) * 1099511628211ULL;
    for (unsigned probe = 0; probe < SLOTS; ++probe) {
        unsigned slot = (unsigned)((hash + probe) % SLOTS);
        if (launches[slot].symbol == NULL) {
            launches[slot].symbol = strdup(symbol);
            launches[slot].count = 1;
            fprintf(records, "{\"event\":\"launched\",\"pid\":%d,\"api\":", getpid());
            json_string(fn);
            fputs(",\"symbol\":", records);
            json_string(symbol);
            fputs("}\n", records);
            fflush(records);
            return;
        }
        if (strcmp(launches[slot].symbol, symbol) == 0) {
            ++launches[slot].count;
            return;
        }
    }
    ++dropped;
}

static void on_api(CUpti_CallbackDomain domain, CUpti_CallbackId id, const CUpti_CallbackData *call) {
    int enter = call->callbackSite == CUPTI_API_ENTER;
    if (enter && runtime_depth == 0 && call->functionName != NULL)
        count_launch(domain, id, call);
    if (domain == CUPTI_CB_DOMAIN_RUNTIME_API)
        runtime_depth += enter ? 1 : -1;
}

static void CUPTIAPI on_callback(void *user, CUpti_CallbackDomain domain, CUpti_CallbackId id, const void *data) {
    (void)user;
    pthread_mutex_lock(&lock);
    if (domain == CUPTI_CB_DOMAIN_RESOURCE && id == CUPTI_CBID_RESOURCE_MODULE_LOADED)
        on_module((const CUpti_ResourceData *)data);
    else if (domain == CUPTI_CB_DOMAIN_RUNTIME_API || domain == CUPTI_CB_DOMAIN_DRIVER_API)
        on_api(domain, id, (const CUpti_CallbackData *)data);
    pthread_mutex_unlock(&lock);
}

// Launch totals are only complete when the process exits normally; first launches are written eagerly.
static void on_exit_totals(void) {
    pthread_mutex_lock(&lock);
    for (unsigned slot = 0; slot < SLOTS; ++slot) {
        if (launches[slot].symbol == NULL)
            continue;
        fprintf(records, "{\"event\":\"launch_count\",\"pid\":%d,\"symbol\":", getpid());
        json_string(launches[slot].symbol);
        fprintf(records, ",\"count\":%llu}\n", launches[slot].count);
    }
    fprintf(records, "{\"event\":\"exit\",\"pid\":%d,\"graph_launches\":%llu,\"dropped_launches\":%llu}\n", getpid(), graph_launches, dropped);
    fflush(records);
    pthread_mutex_unlock(&lock);
}

int InitializeInjection(void) {
    static int initialized;
    if (initialized)
        return 1;
    initialized = 1;
    dir = getenv("GDBG_CAPTURE_DIR");
    if (dir == NULL)
        return 0;
    char path[4096];
    snprintf(path, sizeof path, "%s/records-%d.jsonl", dir, getpid());
    records = fopen(path, "a");
    if (records == NULL)
        return 0;
    uint32_t version = 0;
    cuptiGetVersion(&version);
    fprintf(records, "{\"event\":\"init\",\"pid\":%d,\"cupti_api_version\":%u}\n", getpid(), version);
    fflush(records);

    CUpti_SubscriberHandle subscriber;
    CUptiResult result = cuptiSubscribe(&subscriber, on_callback, NULL);
    if (result != CUPTI_SUCCESS) {
        record_error("cuptiSubscribe", result);
        return 1;
    }
    const CUpti_CallbackDomain domains[] = {CUPTI_CB_DOMAIN_RESOURCE, CUPTI_CB_DOMAIN_RUNTIME_API, CUPTI_CB_DOMAIN_DRIVER_API};
    for (unsigned i = 0; i < sizeof domains / sizeof domains[0]; ++i)
        if ((result = cuptiEnableDomain(1, subscriber, domains[i])) != CUPTI_SUCCESS)
            record_error("cuptiEnableDomain", result);
    atexit(on_exit_totals);
    return 1;
}
