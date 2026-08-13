#define _POSIX_C_SOURCE 200809L

#include <assert.h>
#include <errno.h>
#include <stdint.h>
#include <stddef.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>
#include <poll.h>

void *tn_runtime_alloc(size_t size);
void tn_runtime_free(void *pointer);
uint64_t tn_runtime_free_count(void);
void tn_runtime_reset_allocation_count(void);
void *tn_ref_alloc(size_t size);
void *tn_ref_retain(void *pointer);
int tn_ref_release(void *pointer);
void *tn_ref_downgrade(void *pointer);
void *tn_ref_upgrade(void *pointer);
int tn_ref_release_weak(void *pointer);
void *tn_map_create(size_t key_size, size_t value_size, int ordered);
int tn_map_insert(void *map, const void *key, const void *value);
int tn_map_get(void *map, const void *key, void *value);
int tn_map_contains(void *map, const void *key);
int tn_map_remove(void *map, const void *key);
size_t tn_map_length(void *map);
int tn_map_next(void *map, size_t *cursor, void *key, void *value);
int tn_map_destroy(void *map);
void *tn_channel_create(size_t element_size, size_t capacity);
int tn_channel_send(void *channel, const void *value);
int tn_channel_receive(void *channel, void *value);
int tn_channel_close(void *channel);
int tn_channel_destroy(void *channel);
ssize_t tn_file_read_exact(int handle, void *bytes, size_t length);
ssize_t tn_file_write_all(int handle, const void *bytes, size_t length);
ssize_t tn_net_read_exact(int handle, void *bytes, size_t length);
ssize_t tn_net_write_all(int handle, const void *bytes, size_t length);
void *tn_promise_create(void);
int tn_promise_resolve(void *promise, void *value);
int tn_promise_wait_result(void *promise);
void *tn_promise_take(void *promise, int *failed);
int tn_promise_destroy(void *promise);
void tn_thread_sleep_ns(uint64_t nanoseconds);
void *tn_task_group_create(void);
int tn_task_group_cancel(void *group);
int tn_task_group_is_cancelled(void *group);
int tn_task_group_enter(void *group);
int tn_task_group_leave(void *group);
int tn_task_group_wait(void *group);
int tn_task_group_destroy(void *group);
void *tn_reactor_create(void);
int tn_reactor_watch(void *reactor, int fd, short events);
int tn_reactor_unwatch(void *reactor, int fd);
int tn_reactor_wait(void *reactor, int timeout, int *ready, short *events);
int tn_reactor_destroy(void *reactor);
int32_t tn_selfhost_eval_i32_program_with_parameters(const uint8_t *operations, size_t count,
                                                     const int32_t *parameters, size_t parameter_count,
                                                     int32_t *value);
int32_t tn_selfhost_eval_i32_program(const uint8_t *operations, size_t count, int32_t *value);
int32_t tn_selfhost_llvm_emit_i32_program(const char *output_path, const char *module_name,
                                         const char *function_name, const uint8_t *operations, size_t count);
uint64_t tn_selfhost_hash_declaration(const uint8_t *source, size_t length, size_t start, size_t end);
int32_t tn_selfhost_llvm_emit_i32_program_with_parameters_product(const char *output_path,
                                                                  const char *module_name,
                                                                  const char *function_name,
                                                                  const uint8_t *operations, size_t count,
                                                                  size_t parameter_count, int32_t product);
int32_t tn_selfhost_llvm_emit_i32_module_product(const char *output_path, const char *module_name,
                                                 const uint8_t *source, size_t source_length,
                                                 const uint8_t *functions, size_t function_count,
                                                 const uint8_t *operations, size_t operation_count,
                                                 const char *entry_name, int32_t entry_returns_void,
                                                 int32_t product);

struct selfhost_i32_operation {
  int32_t kind;
  int32_t value;
};

static void *send_value(void *argument) {
  void *channel = argument;
  uint32_t value = 42;
  assert(tn_channel_send(channel, &value) == 0);
  return NULL;
}

static void *retain_and_release(void *argument) {
  void *pointer = argument;
  for (size_t iteration = 0; iteration < 10000; ++iteration) {
    assert(tn_ref_retain(pointer) == pointer);
    assert(tn_ref_release(pointer) == 0);
  }
  return NULL;
}

struct promise_wait_argument {
  void *promise;
  int status;
};

static void *wait_promise(void *argument) {
  struct promise_wait_argument *value = argument;
  value->status = tn_promise_wait_result(value->promise);
  return NULL;
}

static void test_map(void) {
  uint64_t key0 = 0;
  uint64_t key1 = 16;
  uint32_t value0 = 7;
  uint32_t value1 = 8;
  void *map = tn_map_create(sizeof(key0), sizeof(value0), 0);
  assert(map != NULL);
  assert(tn_map_insert(map, &key0, &value0) == 0);
  assert(tn_map_insert(map, &key1, &value1) == 0);
  assert(tn_map_contains(map, &key0) == 1);
  assert(tn_map_contains(map, &key1) == 1);
  assert(tn_map_remove(map, &key0) == 1);
  assert(tn_map_contains(map, &key0) == 0);
  uint32_t output = 0;
  assert(tn_map_get(map, &key1, &output) == 1);
  assert(output == value1);
  size_t cursor = 0;
  uint64_t iterated_key = 0;
  uint32_t iterated_value = 0;
  assert(tn_map_next(map, &cursor, &iterated_key, &iterated_value) == 1);
  assert(iterated_key == key1 && iterated_value == value1);
  assert(tn_map_next(map, &cursor, &iterated_key, &iterated_value) == 0);
  assert(tn_map_length(map) == 1);
  assert(tn_map_destroy(map) == 0);

  map = tn_map_create(sizeof(key0), 0, 1);
  assert(map != NULL);
  assert(tn_map_insert(map, &key0, NULL) == 0);
  assert(tn_map_insert(map, &key1, NULL) == 0);
  assert(tn_map_contains(map, &key1) == 1);
  assert(tn_map_remove(map, &key0) == 1);
  assert(tn_map_contains(map, &key1) == 1);
  assert(tn_map_destroy(map) == 0);

  map = tn_map_create(sizeof(key0), 0, 1);
  assert(map != NULL);
  for (uint64_t index = 31; index > 0; --index) {
    assert(tn_map_insert(map, &index, NULL) == 0);
  }
  assert(tn_map_length(map) == 31);
  cursor = 0;
  uint64_t previous = 0;
  for (size_t index = 0; index < 31; ++index) {
    uint64_t current = 0;
    assert(tn_map_next(map, &cursor, &current, NULL) == 1);
    assert(index == 0 || current > previous);
    previous = current;
  }
  assert(tn_map_next(map, &cursor, &previous, NULL) == 0);
  assert(tn_map_destroy(map) == 0);

  setenv("TN_ALLOC_FAIL_AFTER", "0", 1);
  tn_runtime_reset_allocation_count();
  assert(tn_map_create(sizeof(key0), sizeof(value0), 0) == NULL);
  unsetenv("TN_ALLOC_FAIL_AFTER");
  tn_runtime_reset_allocation_count();
  map = tn_map_create(sizeof(key0), sizeof(value0), 0);
  assert(map != NULL);
  setenv("TN_ALLOC_FAIL_AFTER", "1", 1);
  tn_runtime_reset_allocation_count();
  assert(tn_map_insert(map, &key0, &value0) == -12);
  assert(tn_map_length(map) == 0);
  assert(tn_map_destroy(map) == 0);
  unsetenv("TN_ALLOC_FAIL_AFTER");
}

static void test_refcounts(void) {
  tn_runtime_reset_allocation_count();
  void *pointer = tn_ref_alloc(32);
  assert(pointer != NULL);
  assert(tn_ref_retain(pointer) == pointer);
  assert(tn_ref_release_weak(pointer) == -EINVAL);
  void *weak = tn_ref_downgrade(pointer);
  assert(weak == pointer);
  assert(tn_ref_release(pointer) == 0);
  assert(tn_ref_release(pointer) == 1);
  assert(tn_ref_upgrade(weak) == NULL);
  assert(tn_ref_release_weak(weak) == 1);
  assert(tn_runtime_free_count() == 1);

  pointer = tn_ref_alloc(8);
  assert(tn_ref_retain(pointer) == pointer);
  assert(tn_ref_release(pointer) == 0);
  assert(tn_ref_release(pointer) == 1);
  assert(tn_runtime_free_count() == 2);

  pointer = tn_ref_alloc(16);
  assert(pointer != NULL);
  pthread_t retainers[8];
  for (size_t index = 0; index < 8; ++index) {
    assert(pthread_create(&retainers[index], NULL, retain_and_release, pointer) == 0);
  }
  for (size_t index = 0; index < 8; ++index) {
    assert(pthread_join(retainers[index], NULL) == 0);
  }
  assert(tn_ref_release(pointer) == 1);
  assert(tn_runtime_free_count() == 3);
}

static void test_channel_and_promise(void) {
  void *channel = tn_channel_create(sizeof(uint32_t), 0);
  assert(channel != NULL);
  pthread_t sender;
  assert(pthread_create(&sender, NULL, send_value, channel) == 0);
  uint32_t value = 0;
  assert(tn_channel_receive(channel, &value) == 1);
  assert(value == 42);
  assert(pthread_join(sender, NULL) == 0);
  assert(tn_channel_close(channel) == 0);
  assert(tn_channel_receive(channel, &value) == 0);
  assert(tn_channel_destroy(channel) == 0);

  void *promise = tn_promise_create();
  assert(promise != NULL);
  uint32_t result = 9;
  assert(tn_promise_resolve(promise, &result) == 0);
  assert(tn_promise_wait_result(promise) == 0);
  int failed = 1;
  assert(tn_promise_take(promise, &failed) == &result);
  assert(failed == 0);

  promise = tn_promise_create();
  assert(promise != NULL);
  assert(tn_promise_destroy(promise) == 0);

  promise = tn_promise_create();
  assert(promise != NULL);
  struct promise_wait_argument waiter = {.promise = promise, .status = 0};
  pthread_t waiter_thread;
  assert(pthread_create(&waiter_thread, NULL, wait_promise, &waiter) == 0);
  tn_thread_sleep_ns(1000000);
  assert(tn_promise_destroy(promise) == 0);
  assert(pthread_join(waiter_thread, NULL) == 0);
  assert(waiter.status == 1);

  void *group = tn_task_group_create();
  assert(group != NULL);
  assert(tn_task_group_enter(group) == 0);
  assert(tn_task_group_leave(group) == 0);
  assert(tn_task_group_wait(group) == 0);
  assert(tn_task_group_cancel(group) == 0);
  assert(tn_task_group_is_cancelled(group) != 0);
  assert(tn_task_group_enter(group) != 0);
  assert(tn_task_group_destroy(group) == 0);

  int descriptors[2];
  assert(pipe(descriptors) == 0);
  void *reactor = tn_reactor_create();
  assert(reactor != NULL);
  assert(tn_reactor_watch(reactor, descriptors[0], POLLIN) == 0);
  uint8_t byte = 7;
  assert(write(descriptors[1], &byte, sizeof(byte)) == (ssize_t)sizeof(byte));
  int ready = -1;
  short events = 0;
  assert(tn_reactor_wait(reactor, 1000, &ready, &events) == 1);
  assert(ready == descriptors[0] && (events & POLLIN) != 0);
  assert(tn_reactor_unwatch(reactor, descriptors[0]) == 0);
  assert(tn_reactor_destroy(reactor) == 0);
  close(descriptors[0]);
  close(descriptors[1]);
}

static void test_reliable_io(void) {
  char path[] = "/tmp/typenative-runtime-XXXXXX";
  int file = mkstemp(path);
  assert(file >= 0);
  const char text[] = "reliable file output";
  assert(tn_file_write_all(file, text, sizeof(text) - 1) == 0);
  assert(lseek(file, 0, SEEK_SET) == 0);
  char output[sizeof(text)] = {0};
  assert(tn_file_read_exact(file, output, sizeof(text) - 1) == 0);
  assert(memcmp(output, text, sizeof(text) - 1) == 0);
  assert(close(file) == 0);
  assert(unlink(path) == 0);

  int sockets[2];
  assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
  const char message[] = "reliable network output";
  assert(tn_net_write_all(sockets[0], message, sizeof(message) - 1) == 0);
  char received[sizeof(message)] = {0};
  assert(tn_net_read_exact(sockets[1], received, sizeof(message) - 1) == 0);
  assert(memcmp(received, message, sizeof(message) - 1) == 0);
  assert(close(sockets[0]) == 0);
  assert(close(sockets[1]) == 0);
}

static void test_selfhost_parameter_evaluation(void) {
  const struct selfhost_i32_operation operations[] = {
      {.kind = 8, .value = 0},
      {.kind = 0, .value = 2},
      {.kind = 2, .value = 0},
  };
  const int32_t parameters[] = {40};
  int32_t value = 0;
  assert(tn_selfhost_eval_i32_program_with_parameters(
             (const uint8_t *)operations, sizeof(operations) / sizeof(operations[0]), parameters,
             sizeof(parameters) / sizeof(parameters[0]), &value) == 0);
  assert(value == 42);
  assert(tn_selfhost_eval_i32_program_with_parameters(
             (const uint8_t *)operations, sizeof(operations) / sizeof(operations[0]), NULL, 0, &value) ==
         -EINVAL);
}

static void test_selfhost_parameter_llvm_emission(void) {
  const struct selfhost_i32_operation operations[] = {
      {.kind = 8, .value = 0},
      {.kind = 0, .value = 2},
      {.kind = 2, .value = 0},
  };
  char path[] = "/tmp/typenative-llvm-parameters-XXXXXX";
  const int handle = mkstemp(path);
  assert(handle >= 0);
  assert(close(handle) == 0);
  assert(tn_selfhost_llvm_emit_i32_program_with_parameters_product(
             path, "typenative_parameter_test", "add", (const uint8_t *)operations,
             sizeof(operations) / sizeof(operations[0]), 1, 0) == 0);
  const int input = open(path, O_RDONLY);
  assert(input >= 0);
  char output[4096] = {0};
  const ssize_t length = read(input, output, sizeof(output) - 1);
  assert(length > 0);
  assert(close(input) == 0);
  assert(unlink(path) == 0);
  assert(strstr(output, "define i32 @add") != NULL);
  assert(strstr(output, "add i32") != NULL);
}

static void test_selfhost_conditional_selection(void) {
  const struct selfhost_i32_operation operations[] = {
      {.kind = 0, .value = 1}, {.kind = 0, .value = 7}, {.kind = 0, .value = 9}, {.kind = 11, .value = 0},
  };
  int32_t value = 0;
  assert(tn_selfhost_eval_i32_program(
             (const uint8_t *)operations, sizeof(operations) / sizeof(operations[0]), &value) == 0);
  assert(value == 7);

  struct selfhost_i32_operation false_operations[sizeof(operations) / sizeof(operations[0])];
  memcpy(false_operations, operations, sizeof(operations));
  false_operations[0].value = 0;
  assert(tn_selfhost_eval_i32_program(
             (const uint8_t *)false_operations, sizeof(false_operations) / sizeof(false_operations[0]), &value) == 0);
  assert(value == 9);

  const struct selfhost_i32_operation llvm_operations[] = {
      {.kind = 7, .value = 0}, {.kind = 0, .value = 0}, {.kind = 13, .value = 0},
      {.kind = 0, .value = 7}, {.kind = 0, .value = 9}, {.kind = 11, .value = 0},
  };
  char path[] = "/tmp/typenative-llvm-select-XXXXXX";
  const int handle = mkstemp(path);
  assert(handle >= 0);
  assert(close(handle) == 0);
  assert(tn_selfhost_llvm_emit_i32_program(
             path, "typenative_select_test", "choose", (const uint8_t *)llvm_operations,
             sizeof(llvm_operations) / sizeof(llvm_operations[0])) == 0);
  const int input = open(path, O_RDONLY);
  assert(input >= 0);
  char output[4096] = {0};
  const ssize_t length = read(input, output, sizeof(output) - 1);
  assert(length > 0);
  assert(close(input) == 0);
  assert(unlink(path) == 0);
  assert(strstr(output, "zext i1") != NULL);
  assert(strstr(output, "select i1") != NULL);

  const struct selfhost_i32_operation comparison[] = {
      {.kind = 0, .value = 4}, {.kind = 0, .value = 7}, {.kind = 14, .value = 0},
  };
  assert(tn_selfhost_eval_i32_program(
             (const uint8_t *)comparison, sizeof(comparison) / sizeof(comparison[0]), &value) == 0);
  assert(value == 1);
}

static void test_selfhost_module_llvm_emission(void) {
  const uint8_t source[] = "function add(): i32 { return 40i32; } function main(): i32 { return add(); }";
  const size_t add_name_start = 9;
  const size_t add_name_end = 12;
  const size_t add_body_start = 20;
  const size_t add_body_end = 37;
  const size_t main_name_start = 47;
  const size_t main_name_end = 51;
  const size_t main_body_start = 59;
  const size_t main_body_end = sizeof(source) - 1;
  const size_t functions[] = {
      add_name_start, add_name_end, add_body_start, add_body_end, 0, 14, 17, 2, 1, 1, 0, 1, 1, 1,
      main_name_start, main_name_end, main_body_start, main_body_end, 0, 55, 58, 2, 1, 1, 1, 1, 1, 1,
  };
  const struct selfhost_i32_operation operations[] = {
      {.kind = 0, .value = 40},
      {.kind = 9, .value = 0},
  };
  char path[] = "/tmp/typenative-llvm-module-XXXXXX";
  const int handle = mkstemp(path);
  assert(handle >= 0);
  assert(close(handle) == 0);
  assert(tn_selfhost_llvm_emit_i32_module_product(
             path, "typenative_module_test", source, sizeof(source) - 1,
             (const uint8_t *)functions, 2, (const uint8_t *)operations,
             sizeof(operations) / sizeof(operations[0]), "main", 0, 0) == 0);
  const int input = open(path, O_RDONLY);
  assert(input >= 0);
  char output[8192] = {0};
  const ssize_t length = read(input, output, sizeof(output) - 1);
  assert(length > 0);
  assert(close(input) == 0);
  assert(unlink(path) == 0);
  assert(strstr(output, "define i32 @add") != NULL);
  assert(strstr(output, "define i32 @main") != NULL);
  assert(strstr(output, "call i32 @add()") != NULL);
}

static void test_selfhost_module_parameter_llvm_emission(void) {
  const uint8_t source[] =
      "function add(value: i32): i32 { return value + 2i32; } function main(): i32 { return add(40i32); }";
  const size_t functions[] = {
      9, 12, 30, 54, 1, 26, 29, 2, 1, 1, 0, 1, 3, 1,
      64, 68, 76, 98, 0, 72, 75, 2, 1, 1, 1, 1, 2, 1,
  };
  const struct selfhost_i32_operation operations[] = {
      {.kind = 8, .value = 0}, {.kind = 0, .value = 2}, {.kind = 2, .value = 0},
      {.kind = 0, .value = 40}, {.kind = 9, .value = 0},
  };
  char path[] = "/tmp/typenative-llvm-module-parameters-XXXXXX";
  const int handle = mkstemp(path);
  assert(handle >= 0);
  assert(close(handle) == 0);
  assert(tn_selfhost_llvm_emit_i32_module_product(
             path, "typenative_module_parameter_test", source, sizeof(source) - 1,
             (const uint8_t *)functions, 2, (const uint8_t *)operations,
             sizeof(operations) / sizeof(operations[0]), "main", 0, 0) == 0);
  const int input = open(path, O_RDONLY);
  assert(input >= 0);
  char output[8192] = {0};
  const ssize_t length = read(input, output, sizeof(output) - 1);
  assert(length > 0);
  assert(close(input) == 0);
  assert(unlink(path) == 0);
  assert(strstr(output, "define i32 @add(i32") != NULL);
  assert(strstr(output, "call i32 @add(i32 40)") != NULL);
}

static void test_selfhost_module_void_call_llvm_emission(void) {
  const uint8_t source[] = "function helper(): void {} function main(): void { helper(); }";
  const size_t functions[] = {
      9, 15, 24, 26, 0, 19, 23, 1, 0, 0, 0, 1, 0, 1,
      36, 40, 49, 62, 0, 44, 48, 1, 1, 0, 1, 1, 1, 1,
  };
  const struct selfhost_i32_operation operations[] = {{.kind = 10, .value = 0}};
  char path[] = "/tmp/typenative-llvm-module-void-XXXXXX";
  const int handle = mkstemp(path);
  assert(handle >= 0);
  assert(close(handle) == 0);
  assert(tn_selfhost_llvm_emit_i32_module_product(
             path, "typenative_module_void_test", source, sizeof(source) - 1,
             (const uint8_t *)functions, 2, (const uint8_t *)operations,
             sizeof(operations) / sizeof(operations[0]), "main", 1, 0) == 0);
  const int input = open(path, O_RDONLY);
  assert(input >= 0);
  char output[8192] = {0};
  const ssize_t length = read(input, output, sizeof(output) - 1);
  assert(length > 0);
  assert(close(input) == 0);
  assert(unlink(path) == 0);
  assert(strstr(output, "define void @helper") != NULL);
  assert(strstr(output, "define void @main") != NULL);
  assert(strstr(output, "call void @helper()") != NULL);
}

static void test_selfhost_module_comparison_llvm_emission(void) {
  const uint8_t source[] = "function main(): i32 { return argumentCount() !== 0i32 ? 7i32 : 9i32; }";
  const size_t functions[] = {
      9, 13, 21, sizeof(source) - 1, 0, 17, 20, 2, 1, 1, 1, 1, 6, 1,
  };
  const struct selfhost_i32_operation operations[] = {
      {.kind = 7, .value = 0}, {.kind = 0, .value = 0}, {.kind = 13, .value = 0},
      {.kind = 0, .value = 7}, {.kind = 0, .value = 9}, {.kind = 11, .value = 0},
  };
  char path[] = "/tmp/typenative-llvm-module-comparison-XXXXXX";
  const int handle = mkstemp(path);
  assert(handle >= 0);
  assert(close(handle) == 0);
  assert(tn_selfhost_llvm_emit_i32_module_product(
             path, "typenative_module_comparison_test", source, sizeof(source) - 1,
             (const uint8_t *)functions, 1, (const uint8_t *)operations,
             sizeof(operations) / sizeof(operations[0]), "main", 0, 0) == 0);
  const int input = open(path, O_RDONLY);
  assert(input >= 0);
  char output[8192] = {0};
  const ssize_t length = read(input, output, sizeof(output) - 1);
  assert(length > 0);
  assert(close(input) == 0);
  assert(unlink(path) == 0);
  assert(strstr(output, "zext i1") != NULL);
  assert(strstr(output, "select i1") != NULL);
}

static void test_selfhost_declaration_identity(void) {
  const uint8_t source[] = "function first(): void {} function second(): void {}";
  const uint64_t first = tn_selfhost_hash_declaration(source, sizeof(source) - 1, 0, 25);
  const uint64_t first_again = tn_selfhost_hash_declaration(source, sizeof(source) - 1, 0, 25);
  const uint64_t second = tn_selfhost_hash_declaration(source, sizeof(source) - 1, 26, sizeof(source) - 1);
  assert(first != 0 && first == first_again && first != second);
  assert(tn_selfhost_hash_declaration(source, sizeof(source) - 1, 4, 4) == 0);
}

int main(void) {
  test_map();
  test_refcounts();
  test_channel_and_promise();
  test_reliable_io();
  test_selfhost_parameter_evaluation();
  test_selfhost_parameter_llvm_emission();
  test_selfhost_conditional_selection();
  test_selfhost_module_llvm_emission();
  test_selfhost_module_parameter_llvm_emission();
  test_selfhost_module_void_call_llvm_emission();
  test_selfhost_module_comparison_llvm_emission();
  test_selfhost_declaration_identity();
  return 0;
}
