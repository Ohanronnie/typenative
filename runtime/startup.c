#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#ifndef TN_ENTRY
#error "TN_ENTRY must name the compiled TypeNative entry function"
#endif

extern void tn_runtime_abort(uint32_t code);
extern void *tn_runtime_alloc(size_t size);
extern void tn_runtime_free(void *pointer);
extern void tn_process_set_args(int argc, char **argv);

#if defined(TN_ENTRY_FALLIBLE_I32) || defined(TN_ENTRY_FALLIBLE_VOID)
static int uncaught_error(void *error) {
  fputs("TypeNative: uncaught recoverable error\n", stderr);
  tn_runtime_free(error);
  return 1;
}
#endif

#if defined(TN_ENTRY_FALLIBLE_I32)
typedef struct {
  uint64_t failed;
  uint64_t payload;
} tn_entry_result;

extern tn_entry_result TN_ENTRY(void);

int main(int argc, char **argv) {
  tn_process_set_args(argc, argv);
  tn_entry_result result = TN_ENTRY();
  return result.failed ? uncaught_error((void *)(uintptr_t)result.payload)
                        : (int32_t)result.payload;
}
#elif defined(TN_ENTRY_FALLIBLE_VOID)
typedef struct {
  uint64_t failed;
  uint64_t payload;
} tn_entry_result;

extern tn_entry_result TN_ENTRY(void);

int main(int argc, char **argv) {
  tn_process_set_args(argc, argv);
  tn_entry_result result = TN_ENTRY();
  return result.failed ? uncaught_error((void *)(uintptr_t)result.payload) : 0;
}
#elif defined(TN_ENTRY_I32)
extern int32_t TN_ENTRY(void);

int main(int argc, char **argv) {
  tn_process_set_args(argc, argv);
  return TN_ENTRY();
}
#else
extern void TN_ENTRY(void);

int main(int argc, char **argv) {
  tn_process_set_args(argc, argv);
  TN_ENTRY();
  return 0;
}
#endif
