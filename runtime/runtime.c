#define _GNU_SOURCE

#include <stdint.h>
#include <inttypes.h>
#include <stdatomic.h>
#include <stddef.h>
#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <dlfcn.h>
#include <limits.h>
#include <netdb.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#if defined(__linux__)
#include <sys/epoll.h>
#elif defined(__APPLE__)
#include <sys/event.h>
#endif
#include <spawn.h>
#include <signal.h>
#include <sched.h>
#include <pthread.h>
#include <time.h>
#include <unistd.h>

static _Atomic uint64_t tn_allocation_count;
static _Atomic uint64_t tn_free_count;
static _Atomic uint64_t tn_allocation_fail_after;
static _Atomic int tn_allocation_limit_initialized;

static void tn_initialize_allocation_limit(void) {
  if (atomic_load(&tn_allocation_limit_initialized)) {
    return;
  }
  const char *value = getenv("TN_ALLOC_FAIL_AFTER");
  uint64_t limit = value == NULL ? UINT64_MAX : strtoull(value, NULL, 10);
  int expected = 0;
  if (atomic_compare_exchange_strong(&tn_allocation_limit_initialized, &expected, 2)) {
    atomic_store(&tn_allocation_fail_after, limit);
    atomic_store(&tn_allocation_limit_initialized, 1);
  } else {
    while (atomic_load(&tn_allocation_limit_initialized) == 2) {
      sched_yield();
    }
  }
}

static int tn_allocation_allowed(void) {
  tn_initialize_allocation_limit();
  uint64_t count = atomic_fetch_add(&tn_allocation_count, 1);
  return count < atomic_load(&tn_allocation_fail_after);
}

void tn_runtime_abort(uint32_t code) {
  fprintf(stderr, "TypeNative panic (%u)\n", code);
  abort();
}

void *tn_runtime_alloc(size_t size) {
  if (!tn_allocation_allowed()) {
    fputs("TypeNative: allocation failure\n", stderr);
    abort();
  }
  void *memory = calloc(1, size == 0 ? 1 : size);
  if (memory == NULL) {
    fputs("TypeNative: allocation failure\n", stderr);
    abort();
  }
  return memory;
}

void *tn_runtime_realloc(void *pointer, size_t size) {
  if (!tn_allocation_allowed()) {
    fputs("TypeNative: allocation failure\n", stderr);
    abort();
  }
  void *memory = realloc(pointer, size == 0 ? 1 : size);
  if (memory == NULL) {
    fputs("TypeNative: allocation failure\n", stderr);
    abort();
  }
  return memory;
}

void *tn_runtime_try_realloc(void *pointer, size_t size) {
  if (!tn_allocation_allowed()) {
    return NULL;
  }
  return realloc(pointer, size == 0 ? 1 : size);
}

void *tn_runtime_try_alloc(size_t size) {
  return tn_allocation_allowed() ? calloc(1, size == 0 ? 1 : size) : NULL;
}

void tn_runtime_free(void *pointer) {
  if (pointer != NULL) {
    atomic_fetch_add(&tn_free_count, 1);
  }
  free(pointer);
}

uint64_t tn_runtime_allocation_count(void) { return atomic_load(&tn_allocation_count); }
uint64_t tn_runtime_free_count(void) { return atomic_load(&tn_free_count); }
void tn_runtime_reset_allocation_count(void) {
  atomic_store(&tn_allocation_count, 0);
  atomic_store(&tn_free_count, 0);
  atomic_store(&tn_allocation_limit_initialized, 0);
}

int tn_pointer_is_null(const void *pointer) { return pointer == NULL; }

typedef struct {
  pthread_mutex_t mutex;
  size_t strong;
  size_t weak;
  size_t size;
  unsigned char data[];
} tn_ref_block;

static tn_ref_block *tn_ref_block_for(void *pointer) {
  if (pointer == NULL) {
    return NULL;
  }
  return (tn_ref_block *)((unsigned char *)pointer - offsetof(tn_ref_block, data));
}

void *tn_ref_alloc(size_t size) {
  if (size > SIZE_MAX - sizeof(tn_ref_block)) {
    return NULL;
  }
  tn_ref_block *block = tn_runtime_alloc(sizeof(*block) + size);
  if (pthread_mutex_init(&block->mutex, NULL) != 0) {
    tn_runtime_free(block);
    return NULL;
  }
  block->strong = 1;
  block->weak = 1;
  block->size = size;
  return block->data;
}

void *tn_ref_try_alloc(size_t size) {
  if (size > SIZE_MAX - sizeof(tn_ref_block)) {
    return NULL;
  }
  tn_ref_block *block = tn_runtime_try_alloc(sizeof(*block) + size);
  if (block == NULL) {
    return NULL;
  }
  if (pthread_mutex_init(&block->mutex, NULL) != 0) {
    tn_runtime_free(block);
    return NULL;
  }
  block->strong = 1;
  block->weak = 1;
  block->size = size;
  return block->data;
}

void *tn_ref_retain(void *pointer) {
  tn_ref_block *block = tn_ref_block_for(pointer);
  if (block == NULL) {
    return NULL;
  }
  pthread_mutex_lock(&block->mutex);
  if (block->strong == 0 || block->strong == SIZE_MAX) {
    pthread_mutex_unlock(&block->mutex);
    return NULL;
  }
  ++block->strong;
  pthread_mutex_unlock(&block->mutex);
  return pointer;
}

int tn_ref_release_weak(void *pointer);

static void tn_ref_free_block(tn_ref_block *block) {
  pthread_mutex_destroy(&block->mutex);
  tn_runtime_free(block);
}

int tn_ref_release(void *pointer) {
  tn_ref_block *block = tn_ref_block_for(pointer);
  if (block == NULL) {
    return -EINVAL;
  }
  pthread_mutex_lock(&block->mutex);
  if (block->strong == 0) {
    pthread_mutex_unlock(&block->mutex);
    return -EINVAL;
  }
  --block->strong;
  int last = block->strong == 0;
  int free_block = 0;
  if (last) {
    --block->weak;
    free_block = block->weak == 0;
  }
  pthread_mutex_unlock(&block->mutex);
  if (free_block) {
    tn_ref_free_block(block);
  }
  return last;
}

void *tn_ref_downgrade(void *pointer) {
  tn_ref_block *block = tn_ref_block_for(pointer);
  if (block == NULL) {
    return NULL;
  }
  pthread_mutex_lock(&block->mutex);
  if (block->strong == 0 || block->weak == SIZE_MAX) {
    pthread_mutex_unlock(&block->mutex);
    return NULL;
  }
  ++block->weak;
  pthread_mutex_unlock(&block->mutex);
  return pointer;
}

void *tn_ref_upgrade(void *pointer) {
  tn_ref_block *block = tn_ref_block_for(pointer);
  if (block == NULL) {
    return NULL;
  }
  pthread_mutex_lock(&block->mutex);
  if (block->strong == 0 || block->strong == SIZE_MAX) {
    pthread_mutex_unlock(&block->mutex);
    return NULL;
  }
  ++block->strong;
  pthread_mutex_unlock(&block->mutex);
  return pointer;
}

int tn_ref_release_weak(void *pointer) {
  tn_ref_block *block = tn_ref_block_for(pointer);
  if (block == NULL) {
    return -EINVAL;
  }
  pthread_mutex_lock(&block->mutex);
  if (block->weak == 0 || (block->weak == 1 && block->strong != 0)) {
    pthread_mutex_unlock(&block->mutex);
    return -EINVAL;
  }
  --block->weak;
  int free_block = block->weak == 0;
  pthread_mutex_unlock(&block->mutex);
  if (free_block) {
    tn_ref_free_block(block);
    return 1;
  }
  return 0;
}

int tn_console_write(int fd, const char *bytes, size_t length) {
  ssize_t written = write(fd, bytes, length);
  return written < 0 ? -1 : (int)written;
}

int tn_io_write_all(int fd, const void *bytes, size_t length) {
  const unsigned char *cursor = bytes;
  while (length != 0) {
    ssize_t written = write(fd, cursor, length);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written <= 0) {
      return written == 0 ? -EIO : -errno;
    }
    cursor += (size_t)written;
    length -= (size_t)written;
  }
  return 0;
}

int tn_io_flush(int fd) {
  if (fsync(fd) == 0 || errno == EINVAL || errno == ENOTSUP) {
    return 0;
  }
  return -errno;
}

int tn_file_open_create(const char *path, int flags, int mode) {
  return open(path, flags | O_CREAT, mode);
}

void *tn_dir_open(const char *path) { return opendir(path); }
int tn_dir_next(void *handle, char *name, size_t capacity, uint32_t *kind) {
  if (handle == NULL || name == NULL || capacity == 0) {
    return -EINVAL;
  }
  errno = 0;
  struct dirent *entry = readdir((DIR *)handle);
  if (entry == NULL) {
    return errno == 0 ? 0 : -errno;
  }
  size_t length = strnlen(entry->d_name, capacity);
  if (length + 1 > capacity) {
    return -ENAMETOOLONG;
  }
  memcpy(name, entry->d_name, length);
  name[length] = '\0';
  if (kind != NULL) {
    *kind = (uint32_t)entry->d_type;
  }
  return 1;
}
int tn_dir_close(void *handle) { return handle == NULL ? 0 : closedir((DIR *)handle); }

int tn_process_cwd(char *buffer, size_t capacity) {
  if (buffer == NULL || capacity == 0 || getcwd(buffer, capacity) == NULL) {
    return -errno;
  }
  return 0;
}

int tn_path_join(const char *left, const char *right, char *output, size_t capacity) {
  if (left == NULL || right == NULL || output == NULL || capacity == 0) {
    return -EINVAL;
  }
  int written = snprintf(output, capacity, "%s%s%s", left,
                         (left[0] != '\0' && left[strlen(left) - 1] != '/') ? "/" : "",
                         right);
  if (written < 0 || (size_t)written >= capacity) {
    return -ENAMETOOLONG;
  }
  return 0;
}

int tn_path_basename(const char *path, char *output, size_t capacity) {
  if (path == NULL || output == NULL || capacity == 0) {
    return -EINVAL;
  }
  const char *last = strrchr(path, '/');
  const char *name = last == NULL ? path : last + 1;
  size_t length = strlen(name);
  if (length + 1 > capacity) {
    return -ENAMETOOLONG;
  }
  memcpy(output, name, length + 1);
  return 0;
}

uint64_t tn_clock_monotonic_ns(void) {
  struct timespec value;
  if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
    return 0;
  }
  return (uint64_t)value.tv_sec * UINT64_C(1000000000) + (uint64_t)value.tv_nsec;
}

uint64_t tn_clock_wall_ns(void) {
  struct timespec value;
  if (clock_gettime(CLOCK_REALTIME, &value) != 0) {
    return 0;
  }
  return (uint64_t)value.tv_sec * UINT64_C(1000000000) + (uint64_t)value.tv_nsec;
}

int tn_file_open(const char *path, int flags, int mode) { return open(path, flags, mode); }
ssize_t tn_file_read(int handle, void *bytes, size_t length) { return read(handle, bytes, length); }
ssize_t tn_file_write(int handle, const void *bytes, size_t length) {
  return write(handle, bytes, length);
}
ssize_t tn_file_read_exact(int handle, void *bytes, size_t length) {
  size_t offset = 0;
  while (offset < length) {
    ssize_t received = read(handle, (unsigned char *)bytes + offset, length - offset);
    if (received < 0 && errno == EINTR) {
      continue;
    }
    if (received < 0) {
      return -errno;
    }
    if (received == 0) {
      return -EPIPE;
    }
    offset += (size_t)received;
  }
  return 0;
}
ssize_t tn_file_write_all(int handle, const void *bytes, size_t length) {
  const unsigned char *cursor = bytes;
  while (length != 0) {
    ssize_t written = write(handle, cursor, length);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written < 0) {
      return -errno;
    }
    if (written == 0) {
      return -EIO;
    }
    cursor += (size_t)written;
    length -= (size_t)written;
  }
  return 0;
}
int tn_file_close(int handle) { return close(handle); }

int tn_file_exists(const char *path) { return access(path, F_OK) == 0; }
int tn_file_remove(const char *path) { return unlink(path); }

int tn_file_stat(const char *path, uint64_t *size, uint64_t *modified_ns, uint32_t *mode) {
  struct stat value;
  if (stat(path, &value) != 0) {
    return -errno;
  }
  if (size != NULL) {
    *size = (uint64_t)value.st_size;
  }
  if (modified_ns != NULL) {
    *modified_ns = (uint64_t)value.st_mtime * UINT64_C(1000000000);
  }
  if (mode != NULL) {
    *mode = (uint32_t)value.st_mode;
  }
  return 0;
}

int tn_net_tcp_connect(const char *host, uint16_t port) {
  char service[6];
  (void)snprintf(service, sizeof(service), "%u", (unsigned)port);
  struct addrinfo hints = {0};
  hints.ai_socktype = SOCK_STREAM;
  hints.ai_family = AF_UNSPEC;
  struct addrinfo *addresses = NULL;
  if (getaddrinfo(host, service, &hints, &addresses) != 0) {
    return -1;
  }
  int handle = -1;
  for (struct addrinfo *address = addresses; address != NULL; address = address->ai_next) {
    handle = socket(address->ai_family, address->ai_socktype, address->ai_protocol);
    if (handle >= 0 && connect(handle, address->ai_addr, address->ai_addrlen) == 0) {
      break;
    }
    if (handle >= 0) {
      close(handle);
      handle = -1;
    }
  }
  freeaddrinfo(addresses);
  return handle;
}

int tn_net_tcp_listen(const char *host, uint16_t port, int backlog) {
  int handle = socket(AF_INET, SOCK_STREAM, 0);
  if (handle < 0) {
    return -1;
  }
  int reuse = 1;
  setsockopt(handle, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse));
  struct sockaddr_in address = {0};
  address.sin_family = AF_INET;
  address.sin_port = htons(port);
  if (host == NULL || host[0] == '\0') {
    address.sin_addr.s_addr = htonl(INADDR_ANY);
  } else if (inet_pton(AF_INET, host, &address.sin_addr) != 1) {
    close(handle);
    return -1;
  }
  if (bind(handle, (struct sockaddr *)&address, sizeof(address)) != 0 ||
      listen(handle, backlog > 0 ? backlog : 128) != 0) {
    close(handle);
    return -1;
  }
  return handle;
}

int tn_net_tcp_listen_address(const char *address, int backlog) {
  if (address == NULL) {
    return -1;
  }
  const char *separator = strrchr(address, ':');
  if (separator == NULL || separator == address || separator[1] == '\0') {
    return -1;
  }
  size_t host_length = (size_t)(separator - address);
  if (host_length >= INET6_ADDRSTRLEN) {
    return -1;
  }
  char host[INET6_ADDRSTRLEN];
  memcpy(host, address, host_length);
  host[host_length] = '\0';
  char *end = NULL;
  unsigned long port = strtoul(separator + 1, &end, 10);
  if (end == separator + 1 || *end != '\0' || port > UINT16_MAX) {
    return -1;
  }
  return tn_net_tcp_listen(host, (uint16_t)port, backlog);
}

int tn_net_tcp_accept(int handle) {
  int accepted = accept(handle, NULL, NULL);
  return accepted;
}
int tn_net_set_nonblocking(int handle, int enabled) {
  int flags = fcntl(handle, F_GETFL, 0);
  if (flags < 0) {
    return -errno;
  }
  if (enabled) {
    flags |= O_NONBLOCK;
  } else {
    flags &= ~O_NONBLOCK;
  }
  return fcntl(handle, F_SETFL, flags) == 0 ? 0 : -errno;
}
int tn_net_shutdown(int handle) { return shutdown(handle, SHUT_RDWR); }

ssize_t tn_net_read(int handle, void *bytes, size_t length) { return recv(handle, bytes, length, 0); }
ssize_t tn_net_write(int handle, const void *bytes, size_t length) {
  return send(handle, bytes, length, 0);
}
ssize_t tn_net_read_exact(int handle, void *bytes, size_t length) {
  size_t offset = 0;
  while (offset < length) {
    ssize_t received = recv(handle, (unsigned char *)bytes + offset, length - offset, 0);
    if (received < 0 && errno == EINTR) {
      continue;
    }
    if (received < 0) {
      return -errno;
    }
    if (received == 0) {
      return -EPIPE;
    }
    offset += (size_t)received;
  }
  return 0;
}
ssize_t tn_net_write_all(int handle, const void *bytes, size_t length) {
  const unsigned char *cursor = bytes;
  while (length != 0) {
    ssize_t written = send(handle, cursor, length, 0);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written < 0) {
      return -errno;
    }
    if (written == 0) {
      return -EIO;
    }
    cursor += (size_t)written;
    length -= (size_t)written;
  }
  return 0;
}
int tn_net_close(int handle) {
  return close(handle);
}

int tn_net_read_into(int handle, uint8_t *bytes, size_t capacity, size_t length,
                     size_t *received) {
  if (bytes == NULL || received == NULL || length > capacity) {
    return EINVAL;
  }
  for (;;) {
    ssize_t result = recv(handle, bytes + length, capacity - length, 0);
    if (result < 0 && errno == EINTR) {
      continue;
    }
    if (result < 0) {
      return errno;
    }
    *received = (size_t)result;
    return 0;
  }
}

int tn_net_udp_bind(const char *host, uint16_t port) {
  int handle = socket(AF_INET, SOCK_DGRAM, 0);
  if (handle < 0) {
    return -1;
  }
  struct sockaddr_in address = {0};
  address.sin_family = AF_INET;
  address.sin_port = htons(port);
  if (host == NULL || host[0] == '\0') {
    address.sin_addr.s_addr = htonl(INADDR_ANY);
  } else if (inet_pton(AF_INET, host, &address.sin_addr) != 1) {
    close(handle);
    return -1;
  }
  if (bind(handle, (struct sockaddr *)&address, sizeof(address)) != 0) {
    close(handle);
    return -1;
  }
  return handle;
}

ssize_t tn_net_udp_send(int handle, const void *bytes, size_t length, const char *host,
                        uint16_t port) {
  struct sockaddr_in address = {0};
  address.sin_family = AF_INET;
  address.sin_port = htons(port);
  if (host == NULL || inet_pton(AF_INET, host, &address.sin_addr) != 1) {
    return -1;
  }
  return sendto(handle, bytes, length, 0, (struct sockaddr *)&address, sizeof(address));
}

ssize_t tn_net_udp_recv(int handle, void *bytes, size_t length) {
  return recvfrom(handle, bytes, length, 0, NULL, NULL);
}

int tn_net_resolve_ipv4(const char *host, uint32_t *address) {
  struct addrinfo hints = {0};
  hints.ai_family = AF_INET;
  hints.ai_socktype = SOCK_STREAM;
  struct addrinfo *addresses = NULL;
  if (getaddrinfo(host, NULL, &hints, &addresses) != 0 || addresses == NULL) {
    return -1;
  }
  struct sockaddr_in *value = (struct sockaddr_in *)addresses->ai_addr;
  if (address != NULL) {
    *address = ntohl(value->sin_addr.s_addr);
  }
  freeaddrinfo(addresses);
  return 0;
}

void tn_process_exit(int32_t code) { _exit(code); }
int32_t tn_process_id(void) { return (int32_t)getpid(); }
const char *tn_process_getenv(const char *name) { return getenv(name); }
size_t tn_process_getenv_length(const char *name) {
  const char *value = getenv(name);
  return value == NULL ? SIZE_MAX : strlen(value);
}
int tn_process_getenv_copy(const char *name, uint8_t *destination, size_t capacity) {
  const char *value = getenv(name);
  if (value == NULL || destination == NULL) {
    return -ENOENT;
  }
  size_t length = strlen(value);
  if (length == SIZE_MAX || capacity < length + 1) {
    return -ERANGE;
  }
  memcpy(destination, value, length);
  destination[length] = '\0';
  return 0;
}
static int tn_process_argc_value;
static char **tn_process_argv_value;
void tn_process_set_args(int argc, char **argv) {
  tn_process_argc_value = argc;
  tn_process_argv_value = argv;
}
int32_t tn_process_argc(void) { return tn_process_argc_value; }
const char *tn_process_argv(int32_t index) {
  return index >= 0 && index < tn_process_argc_value ? tn_process_argv_value[index] : NULL;
}
extern char **environ;
int32_t tn_process_spawn(const char *command) {
  if (command == NULL || command[0] == '\0') {
    return -EINVAL;
  }
  pid_t child = 0;
  char *const arguments[] = {(char *)command, NULL};
  int status = posix_spawnp(&child, command, NULL, NULL, arguments, environ);
  return status == 0 ? (int32_t)child : -status;
}
int tn_process_wait(int32_t process, int32_t *exit_code) {
  if (process <= 0) {
    return EINVAL;
  }
  int status = 0;
  if (waitpid((pid_t)process, &status, 0) < 0) {
    return errno;
  }
  if (exit_code != NULL) {
    *exit_code = WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
  }
  return 0;
}
int tn_process_kill(int32_t process, int32_t signal_number) {
  return process > 0 && kill((pid_t)process, signal_number) == 0 ? 0 : errno;
}

typedef void (*tn_async_poll_fn)(void *context, void *result);
typedef void (*tn_async_drop_fn)(void *context);

typedef struct {
  pthread_mutex_t mutex;
  pthread_cond_t condition;
  tn_async_poll_fn poll;
  tn_async_drop_fn drop;
  void *context;
  void *result;
  size_t result_offset;
  int state;
} tn_async_promise;

void *tn_runtime_async_create(
    size_t result_size, size_t result_offset, tn_async_poll_fn poll, void *context,
    tn_async_drop_fn drop) {
  if (poll == NULL) {
    return NULL;
  }
  tn_async_promise *promise = tn_runtime_try_alloc(sizeof(*promise));
  if (promise == NULL) {
    return NULL;
  }
  promise->poll = poll;
  promise->drop = drop;
  promise->result_offset = result_offset;
  promise->context = context;
  promise->result = tn_runtime_try_alloc(result_size == 0 ? 1 : result_size);
  if (promise->result == NULL) {
    if (drop != NULL) {
      drop(context);
    } else {
      tn_runtime_free(context);
    }
    tn_runtime_free(promise);
    return NULL;
  }
  if (pthread_mutex_init(&promise->mutex, NULL) != 0) {
    if (drop != NULL) {
      drop(context);
    } else {
      tn_runtime_free(context);
    }
    tn_runtime_free(promise->result);
    tn_runtime_free(promise);
    return NULL;
  }
  if (pthread_cond_init(&promise->condition, NULL) != 0) {
    pthread_mutex_destroy(&promise->mutex);
    if (drop != NULL) {
      drop(context);
    } else {
      tn_runtime_free(context);
    }
    tn_runtime_free(promise->result);
    tn_runtime_free(promise);
    return NULL;
  }
  return promise;
}

int tn_runtime_async_wait(void *handle) {
  tn_async_promise *promise = handle;
  if (promise == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&promise->mutex);
  if (promise->state == 2) {
    pthread_mutex_unlock(&promise->mutex);
    return 0;
  }
  if (promise->state == 3) {
    pthread_mutex_unlock(&promise->mutex);
    return ECANCELED;
  }
  if (promise->state == 1) {
    while (promise->state == 1) {
      pthread_cond_wait(&promise->condition, &promise->mutex);
    }
    int status = promise->state == 2 ? 0 : ECANCELED;
    pthread_mutex_unlock(&promise->mutex);
    return status;
  }
  promise->state = 1;
  tn_async_poll_fn poll = promise->poll;
  void *context = promise->context;
  void *result = promise->result;
  pthread_mutex_unlock(&promise->mutex);
  poll(context, result);
  pthread_mutex_lock(&promise->mutex);
  if (promise->state == 1) {
    promise->state = 2;
  }
  pthread_cond_broadcast(&promise->condition);
  int status = promise->state == 2 ? 0 : ECANCELED;
  pthread_mutex_unlock(&promise->mutex);
  return status;
}

void *tn_runtime_async_result(void *handle) {
  tn_async_promise *promise = handle;
  return promise == NULL || promise->result == NULL
             ? NULL
             : (unsigned char *)promise->result + promise->result_offset;
}

void *tn_runtime_async_raw_result(void *handle) {
  tn_async_promise *promise = handle;
  return promise == NULL ? NULL : promise->result;
}

int tn_runtime_async_destroy(void *handle) {
  tn_async_promise *promise = handle;
  if (promise == NULL) {
    return 0;
  }
  pthread_mutex_lock(&promise->mutex);
  while (promise->state == 1) {
    pthread_cond_wait(&promise->condition, &promise->mutex);
  }
  int started = promise->state != 0;
  promise->state = 3;
  void *context = promise->context;
  void *result = promise->result;
  tn_async_drop_fn drop = promise->drop;
  promise->context = NULL;
  promise->result = NULL;
  pthread_mutex_unlock(&promise->mutex);
  pthread_cond_destroy(&promise->condition);
  pthread_mutex_destroy(&promise->mutex);
  if (drop != NULL && !started) {
    drop(context);
  } else {
    tn_runtime_free(context);
  }
  tn_runtime_free(result);
  tn_runtime_free(promise);
  return 0;
}

void tn_runtime_promise_wait(void *promise) {
  if (promise == NULL) {
    return;
  }
  (void)tn_runtime_async_wait(promise);
}

int tn_runtime_promise_destroy(void *promise) {
  return tn_runtime_async_destroy(promise);
}

int32_t tn_runtime_promise_take_i32(void *promise) {
  if (promise == NULL) {
    return 0;
  }
  if (tn_runtime_async_wait(promise) != 0) {
    (void)tn_runtime_async_destroy(promise);
    return 0;
  }
  void *result = tn_runtime_async_result(promise);
  int32_t value = result == NULL ? 0 : *(int32_t *)result;
  (void)tn_runtime_async_destroy(promise);
  return value;
}

void tn_thread_sleep_ns(uint64_t nanoseconds) {
  struct timespec value = {
      .tv_sec = (time_t)(nanoseconds / UINT64_C(1000000000)),
      .tv_nsec = (long)(nanoseconds % UINT64_C(1000000000)),
  };
  while (nanosleep(&value, &value) != 0 && errno == EINTR) {
  }
}

void tn_thread_yield(void) { sched_yield(); }

typedef struct {
  pthread_t thread;
} tn_thread_handle;

tn_thread_handle *tn_thread_spawn(void *(*entry)(void *), void *argument) {
  if (entry == NULL) {
    return NULL;
  }
  tn_thread_handle *handle = calloc(1, sizeof(*handle));
  if (handle == NULL || pthread_create(&handle->thread, NULL, entry, argument) != 0) {
    free(handle);
    return NULL;
  }
  return handle;
}

tn_thread_handle *tn_thread_spawn_raw(uintptr_t entry, void *argument) {
  return tn_thread_spawn((void *(*)(void *))entry, argument);
}

int tn_thread_join(tn_thread_handle *handle, void **result) {
  if (handle == NULL) {
    return EINVAL;
  }
  int status = pthread_join(handle->thread, result);
  if (status == 0) {
    free(handle);
  }
  return status;
}

int tn_thread_detach(tn_thread_handle *handle) {
  if (handle == NULL) {
    return EINVAL;
  }
  int status = pthread_detach(handle->thread);
  if (status == 0) {
    free(handle);
  }
  return status;
}

uint64_t tn_thread_id(void) {
  pthread_t self = pthread_self();
  uint64_t id = 0;
  size_t copy = sizeof(self) < sizeof(id) ? sizeof(self) : sizeof(id);
  memcpy(&id, &self, copy);
  return id;
}

void *tn_mutex_create(void) {
  pthread_mutex_t *mutex = calloc(1, sizeof(*mutex));
  if (mutex == NULL || pthread_mutex_init(mutex, NULL) != 0) {
    free(mutex);
    return NULL;
  }
  return mutex;
}
int tn_mutex_lock(void *handle) {
  return handle == NULL ? EINVAL : pthread_mutex_lock((pthread_mutex_t *)handle);
}
int tn_mutex_unlock(void *handle) {
  return handle == NULL ? EINVAL : pthread_mutex_unlock((pthread_mutex_t *)handle);
}
int tn_mutex_destroy(void *handle) {
  if (handle == NULL) {
    return EINVAL;
  }
  int result = pthread_mutex_destroy((pthread_mutex_t *)handle);
  if (result == 0) {
    free(handle);
  }
  return result;
}

void *tn_rwlock_create(void) {
  pthread_rwlock_t *lock = calloc(1, sizeof(*lock));
  if (lock == NULL || pthread_rwlock_init(lock, NULL) != 0) {
    free(lock);
    return NULL;
  }
  return lock;
}
int tn_rwlock_read_lock(void *handle) {
  return handle == NULL ? EINVAL : pthread_rwlock_rdlock((pthread_rwlock_t *)handle);
}
int tn_rwlock_write_lock(void *handle) {
  return handle == NULL ? EINVAL : pthread_rwlock_wrlock((pthread_rwlock_t *)handle);
}
int tn_rwlock_unlock(void *handle) {
  return handle == NULL ? EINVAL : pthread_rwlock_unlock((pthread_rwlock_t *)handle);
}
int tn_rwlock_destroy(void *handle) {
  if (handle == NULL) {
    return EINVAL;
  }
  int result = pthread_rwlock_destroy((pthread_rwlock_t *)handle);
  if (result == 0) {
    free(handle);
  }
  return result;
}

void *tn_cond_create(void) {
  pthread_cond_t *condition = calloc(1, sizeof(*condition));
  if (condition == NULL || pthread_cond_init(condition, NULL) != 0) {
    free(condition);
    return NULL;
  }
  return condition;
}
int tn_cond_wait(void *condition, void *mutex) {
  if (condition == NULL || mutex == NULL) {
    return EINVAL;
  }
  return pthread_cond_wait((pthread_cond_t *)condition, (pthread_mutex_t *)mutex);
}
int tn_cond_signal(void *condition) {
  return condition == NULL ? EINVAL : pthread_cond_signal((pthread_cond_t *)condition);
}
int tn_cond_broadcast(void *condition) {
  return condition == NULL ? EINVAL : pthread_cond_broadcast((pthread_cond_t *)condition);
}
int tn_cond_destroy(void *handle) {
  if (handle == NULL) {
    return EINVAL;
  }
  int result = pthread_cond_destroy((pthread_cond_t *)handle);
  if (result == 0) {
    free(handle);
  }
  return result;
}

typedef struct {
  pthread_mutex_t mutex;
  pthread_cond_t condition;
  size_t threshold;
  size_t waiting;
  uint64_t generation;
} tn_barrier;

void *tn_barrier_create(size_t threshold) {
  if (threshold == 0) {
    return NULL;
  }
  tn_barrier *barrier = calloc(1, sizeof(*barrier));
  if (barrier == NULL) {
    return NULL;
  }
  if (pthread_mutex_init(&barrier->mutex, NULL) != 0) {
    free(barrier);
    return NULL;
  }
  if (pthread_cond_init(&barrier->condition, NULL) != 0) {
    pthread_mutex_destroy(&barrier->mutex);
    free(barrier);
    return NULL;
  }
  barrier->threshold = threshold;
  return barrier;
}

int tn_barrier_wait(void *handle) {
  tn_barrier *barrier = handle;
  if (barrier == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&barrier->mutex);
  uint64_t generation = barrier->generation;
  if (++barrier->waiting == barrier->threshold) {
    barrier->waiting = 0;
    ++barrier->generation;
    pthread_cond_broadcast(&barrier->condition);
    pthread_mutex_unlock(&barrier->mutex);
    return 1;
  }
  while (generation == barrier->generation) {
    pthread_cond_wait(&barrier->condition, &barrier->mutex);
  }
  pthread_mutex_unlock(&barrier->mutex);
  return 0;
}

int tn_barrier_destroy(void *handle) {
  tn_barrier *barrier = handle;
  if (barrier == NULL) {
    return 0;
  }
  int status = pthread_cond_destroy(&barrier->condition);
  if (status == 0) {
    status = pthread_mutex_destroy(&barrier->mutex);
  }
  if (status == 0) {
    free(barrier);
  }
  return status;
}

typedef struct {
  unsigned char *storage;
  size_t element_size;
  size_t capacity;
  size_t length;
  size_t head;
  int unbuffered;
  int closed;
  pthread_mutex_t mutex;
  pthread_cond_t readable;
  pthread_cond_t writable;
} tn_channel;

void *tn_channel_create(size_t element_size, size_t capacity) {
  if (element_size == 0 || (capacity != 0 && element_size > SIZE_MAX / capacity)) {
    return NULL;
  }
  int unbuffered = capacity == 0;
  if (unbuffered) {
    capacity = 1;
  }
  tn_channel *channel = calloc(1, sizeof(*channel));
  if (channel == NULL) {
    return NULL;
  }
  channel->storage = calloc(capacity, element_size);
  if (channel->storage == NULL) {
    free(channel);
    return NULL;
  }
  if (pthread_mutex_init(&channel->mutex, NULL) != 0) {
    free(channel->storage);
    free(channel);
    return NULL;
  }
  if (pthread_cond_init(&channel->readable, NULL) != 0) {
    pthread_mutex_destroy(&channel->mutex);
    free(channel->storage);
    free(channel);
    return NULL;
  }
  if (pthread_cond_init(&channel->writable, NULL) != 0) {
    pthread_cond_destroy(&channel->readable);
    pthread_mutex_destroy(&channel->mutex);
    free(channel->storage);
    free(channel);
    return NULL;
  }
  channel->element_size = element_size;
  channel->capacity = capacity;
  channel->unbuffered = unbuffered;
  return channel;
}

int tn_channel_send(void *handle, const void *value) {
  tn_channel *channel = handle;
  if (channel == NULL || value == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&channel->mutex);
  while (channel->length == channel->capacity && !channel->closed) {
    pthread_cond_wait(&channel->writable, &channel->mutex);
  }
  if (channel->closed) {
    pthread_mutex_unlock(&channel->mutex);
    return EPIPE;
  }
  size_t index = (channel->head + channel->length) % channel->capacity;
  memcpy(channel->storage + index * channel->element_size, value, channel->element_size);
  ++channel->length;
  pthread_cond_signal(&channel->readable);
  if (channel->unbuffered) {
    while (channel->length != 0 && !channel->closed) {
      pthread_cond_wait(&channel->writable, &channel->mutex);
    }
    if (channel->closed && channel->length != 0) {
      pthread_mutex_unlock(&channel->mutex);
      return EPIPE;
    }
  }
  pthread_mutex_unlock(&channel->mutex);
  return 0;
}

int tn_channel_receive(void *handle, void *value) {
  tn_channel *channel = handle;
  if (channel == NULL || value == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&channel->mutex);
  while (channel->length == 0 && !channel->closed) {
    pthread_cond_wait(&channel->readable, &channel->mutex);
  }
  if (channel->length == 0) {
    pthread_mutex_unlock(&channel->mutex);
    return channel->closed ? 0 : EAGAIN;
  }
  memcpy(value, channel->storage + channel->head * channel->element_size, channel->element_size);
  channel->head = (channel->head + 1) % channel->capacity;
  --channel->length;
  pthread_cond_signal(&channel->writable);
  pthread_mutex_unlock(&channel->mutex);
  return 1;
}

int tn_channel_close(void *handle) {
  tn_channel *channel = handle;
  if (channel == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&channel->mutex);
  channel->closed = 1;
  pthread_cond_broadcast(&channel->readable);
  pthread_cond_broadcast(&channel->writable);
  pthread_mutex_unlock(&channel->mutex);
  return 0;
}

int tn_channel_destroy(void *handle) {
  tn_channel *channel = handle;
  if (channel == NULL) {
    return 0;
  }
  int status = pthread_cond_destroy(&channel->readable);
  if (status == 0) {
    status = pthread_cond_destroy(&channel->writable);
  }
  if (status == 0) {
    status = pthread_mutex_destroy(&channel->mutex);
  }
  if (status == 0) {
    free(channel->storage);
    free(channel);
  }
  return status;
}

int32_t tn_atomic_i32_load(const int32_t *value) {
  return value == NULL ? 0 : atomic_load((_Atomic int32_t *)value);
}
int32_t tn_atomic_i32_fetch_add(int32_t *value, int32_t delta) {
  return value == NULL ? 0 : atomic_fetch_add((_Atomic int32_t *)value, delta);
}
int32_t tn_atomic_i32_store(int32_t *value, int32_t next) {
  if (value == NULL) {
    return EINVAL;
  }
  atomic_store((_Atomic int32_t *)value, next);
  return next;
}
int32_t tn_atomic_i32_compare_exchange(int32_t *value, int32_t *expected, int32_t next) {
  if (value == NULL || expected == NULL) {
    return 0;
  }
  return atomic_compare_exchange_strong((_Atomic int32_t *)value, expected, next);
}
uint64_t tn_atomic_u64_load(const uint64_t *value) {
  return value == NULL ? 0 : atomic_load((_Atomic uint64_t *)value);
}
uint64_t tn_atomic_u64_fetch_add(uint64_t *value, uint64_t delta) {
  return value == NULL ? 0 : atomic_fetch_add((_Atomic uint64_t *)value, delta);
}
uint64_t tn_atomic_u64_store(uint64_t *value, uint64_t next) {
  if (value == NULL) {
    return 0;
  }
  atomic_store((_Atomic uint64_t *)value, next);
  return next;
}
int32_t tn_atomic_u64_compare_exchange(uint64_t *value, uint64_t *expected, uint64_t next) {
  if (value == NULL || expected == NULL) {
    return 0;
  }
  return atomic_compare_exchange_strong((_Atomic uint64_t *)value, expected, next);
}
size_t tn_atomic_usize_load(const size_t *value) {
  return value == NULL ? 0 : atomic_load((_Atomic size_t *)value);
}
size_t tn_atomic_usize_fetch_add(size_t *value, size_t delta) {
  return value == NULL ? 0 : atomic_fetch_add((_Atomic size_t *)value, delta);
}
size_t tn_atomic_usize_store(size_t *value, size_t next) {
  if (value == NULL) {
    return 0;
  }
  atomic_store((_Atomic size_t *)value, next);
  return next;
}
int32_t tn_atomic_usize_compare_exchange(size_t *value, size_t *expected, size_t next) {
  if (value == NULL || expected == NULL) {
    return 0;
  }
  return atomic_compare_exchange_strong((_Atomic size_t *)value, expected, next);
}

typedef struct {
  pthread_mutex_t mutex;
  pthread_cond_t condition;
  void *value;
  void *error;
  size_t waiters;
  int ready;
  int failed;
  int consumed;
} tn_promise;

void *tn_promise_create(void) {
  tn_promise *promise = calloc(1, sizeof(*promise));
  if (promise == NULL) {
    return NULL;
  }
  if (pthread_mutex_init(&promise->mutex, NULL) != 0) {
    free(promise);
    return NULL;
  }
  if (pthread_cond_init(&promise->condition, NULL) != 0) {
    pthread_mutex_destroy(&promise->mutex);
    free(promise);
    return NULL;
  }
  return promise;
}

static int tn_promise_complete(tn_promise *promise, void *value, void *error, int failed) {
  if (promise == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&promise->mutex);
  if (promise->ready || promise->consumed) {
    pthread_mutex_unlock(&promise->mutex);
    return EALREADY;
  }
  promise->value = value;
  promise->error = error;
  promise->failed = failed;
  promise->ready = 1;
  pthread_cond_broadcast(&promise->condition);
  pthread_mutex_unlock(&promise->mutex);
  return 0;
}

int tn_promise_resolve(void *handle, void *value) {
  return tn_promise_complete(handle, value, NULL, 0);
}
int tn_promise_reject(void *handle, void *error) {
  return tn_promise_complete(handle, NULL, error, 1);
}

int tn_promise_cancel(void *handle) {
  return tn_promise_complete(handle, NULL, NULL, 1);
}

int tn_promise_wait(void *handle) {
  tn_promise *promise = handle;
  if (promise == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&promise->mutex);
  ++promise->waiters;
  while (!promise->ready) {
    pthread_cond_wait(&promise->condition, &promise->mutex);
  }
  int failed = promise->failed;
  --promise->waiters;
  pthread_cond_broadcast(&promise->condition);
  pthread_mutex_unlock(&promise->mutex);
  return failed ? 1 : 0;
}
int tn_promise_wait_result(void *handle) { return tn_promise_wait(handle); }

void *tn_promise_take(void *handle, int *failed) {
  tn_promise *promise = handle;
  if (promise == NULL) {
    return NULL;
  }
  pthread_mutex_lock(&promise->mutex);
  ++promise->waiters;
  while (!promise->ready) {
    pthread_cond_wait(&promise->condition, &promise->mutex);
  }
  if (promise->consumed) {
    --promise->waiters;
    pthread_cond_broadcast(&promise->condition);
    pthread_mutex_unlock(&promise->mutex);
    return NULL;
  }
  void *value = promise->failed ? promise->error : promise->value;
  if (failed != NULL) {
    *failed = promise->failed;
  }
  promise->consumed = 1;
  --promise->waiters;
  pthread_cond_broadcast(&promise->condition);
  while (promise->waiters != 0) {
    pthread_cond_wait(&promise->condition, &promise->mutex);
  }
  pthread_mutex_unlock(&promise->mutex);
  pthread_cond_destroy(&promise->condition);
  pthread_mutex_destroy(&promise->mutex);
  free(promise);
  return value;
}

int tn_promise_destroy(void *handle) {
  tn_promise *promise = handle;
  if (promise == NULL) {
    return 0;
  }
  pthread_mutex_lock(&promise->mutex);
  if (promise->consumed) {
    pthread_mutex_unlock(&promise->mutex);
    return EALREADY;
  }
  if (!promise->ready) {
    promise->ready = 1;
    promise->failed = 1;
    pthread_cond_broadcast(&promise->condition);
  }
  while (promise->waiters != 0) {
    pthread_cond_wait(&promise->condition, &promise->mutex);
  }
  pthread_mutex_unlock(&promise->mutex);
  pthread_cond_destroy(&promise->condition);
  pthread_mutex_destroy(&promise->mutex);
  free(promise);
  return 0;
}

typedef struct {
  _Atomic int cancelled;
  pthread_mutex_t mutex;
  pthread_cond_t condition;
  size_t active;
} tn_task_group;

void *tn_task_group_create(void) {
  tn_task_group *group = calloc(1, sizeof(*group));
  if (group != NULL) {
    atomic_init(&group->cancelled, 0);
    if (pthread_mutex_init(&group->mutex, NULL) != 0) {
      free(group);
      return NULL;
    }
    if (pthread_cond_init(&group->condition, NULL) != 0) {
      pthread_mutex_destroy(&group->mutex);
      free(group);
      return NULL;
    }
  }
  return group;
}

int tn_task_group_cancel(void *handle) {
  tn_task_group *group = handle;
  if (group == NULL) {
    return EINVAL;
  }
  atomic_store(&group->cancelled, 1);
  return 0;
}

int tn_task_group_is_cancelled(void *handle) {
  tn_task_group *group = handle;
  return group == NULL ? 1 : atomic_load(&group->cancelled);
}

int tn_task_group_enter(void *handle) {
  tn_task_group *group = handle;
  if (group == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&group->mutex);
  if (atomic_load(&group->cancelled)) {
    pthread_mutex_unlock(&group->mutex);
    return ECANCELED;
  }
  if (group->active == SIZE_MAX) {
    pthread_mutex_unlock(&group->mutex);
    return EOVERFLOW;
  }
  ++group->active;
  pthread_mutex_unlock(&group->mutex);
  return 0;
}

int tn_task_group_leave(void *handle) {
  tn_task_group *group = handle;
  if (group == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&group->mutex);
  if (group->active == 0) {
    pthread_mutex_unlock(&group->mutex);
    return EINVAL;
  }
  --group->active;
  if (group->active == 0) {
    pthread_cond_broadcast(&group->condition);
  }
  pthread_mutex_unlock(&group->mutex);
  return 0;
}

typedef struct {
  tn_task_group *group;
  void *promise;
} tn_task_worker;

static void *tn_task_worker_entry(void *argument) {
  tn_task_worker *worker = argument;
  if (worker == NULL) {
    return NULL;
  }
  int wait_status = tn_runtime_async_wait(worker->promise);
  int destroy_status = tn_runtime_async_destroy(worker->promise);
  int leave_status = tn_task_group_leave(worker->group);
  (void)wait_status;
  (void)destroy_status;
  (void)leave_status;
  free(worker);
  return NULL;
}

int tn_task_group_spawn(void *handle, void *promise) {
  tn_task_group *group = handle;
  if (group == NULL || promise == NULL) {
    return EINVAL;
  }
  int status = tn_task_group_enter(group);
  if (status != 0) {
    return status;
  }
  tn_task_worker *worker = calloc(1, sizeof(*worker));
  if (worker == NULL) {
    (void)tn_task_group_leave(group);
    return ENOMEM;
  }
  worker->group = group;
  worker->promise = promise;
  pthread_t thread;
  status = pthread_create(&thread, NULL, tn_task_worker_entry, worker);
  if (status != 0) {
    free(worker);
    (void)tn_task_group_leave(group);
    return status;
  }
  status = pthread_detach(thread);
  if (status != 0) {
    pthread_join(thread, NULL);
    return status;
  }
  return 0;
}

int tn_task_group_wait(void *handle) {
  tn_task_group *group = handle;
  if (group == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&group->mutex);
  while (group->active != 0) {
    pthread_cond_wait(&group->condition, &group->mutex);
  }
  pthread_mutex_unlock(&group->mutex);
  return 0;
}

int tn_task_group_destroy(void *handle) {
  tn_task_group *group = handle;
  if (group == NULL) {
    return 0;
  }
  atomic_store(&group->cancelled, 1);
  tn_task_group_wait(group);
  int status = pthread_cond_destroy(&group->condition);
  if (status == 0) {
    status = pthread_mutex_destroy(&group->mutex);
  }
  if (status == 0) {
    free(group);
  }
  return status;
}

typedef struct {
  int handle;
  short events;
} tn_reactor_registration;

typedef struct {
  tn_reactor_registration *registrations;
  size_t length;
  size_t capacity;
  pthread_mutex_t mutex;
  int backend;
} tn_reactor;

void *tn_reactor_create(void) {
  tn_reactor *reactor = calloc(1, sizeof(*reactor));
  if (reactor == NULL || pthread_mutex_init(&reactor->mutex, NULL) != 0) {
    free(reactor);
    return NULL;
  }
#if defined(__linux__)
  reactor->backend = epoll_create1(EPOLL_CLOEXEC);
#elif defined(__APPLE__)
  reactor->backend = kqueue();
#else
  reactor->backend = -1;
#endif
#if defined(__linux__) || defined(__APPLE__)
  if (reactor->backend < 0) {
    pthread_mutex_destroy(&reactor->mutex);
    free(reactor);
    return NULL;
  }
#endif
  return reactor;
}

#if defined(__linux__)
static uint32_t tn_reactor_epoll_events(short events) {
  uint32_t converted = EPOLLERR | EPOLLHUP;
  if ((events & POLLIN) != 0) {
    converted |= EPOLLIN;
  }
  if ((events & POLLOUT) != 0) {
    converted |= EPOLLOUT;
  }
  return converted;
}
#elif defined(__APPLE__)
static int tn_reactor_kevent_update(int backend, int fd, short events, uint16_t flags) {
  struct kevent changes[2];
  int count = 0;
  if ((events & POLLIN) != 0 || flags == EV_DELETE) {
    EV_SET(&changes[count++], (uintptr_t)fd, EVFILT_READ, flags, 0, 0, NULL);
  }
  if ((events & POLLOUT) != 0 || flags == EV_DELETE) {
    EV_SET(&changes[count++], (uintptr_t)fd, EVFILT_WRITE, flags, 0, 0, NULL);
  }
  return count == 0 || kevent(backend, changes, count, NULL, 0, NULL) == 0 ? 0 : errno;
}
#endif

int tn_reactor_watch(void *handle, int fd, short events) {
  tn_reactor *reactor = handle;
  if (reactor == NULL || fd < 0) {
    return EINVAL;
  }
  pthread_mutex_lock(&reactor->mutex);
  for (size_t index = 0; index < reactor->length; ++index) {
    if (reactor->registrations[index].handle == fd) {
#if defined(__linux__)
      struct epoll_event event = {.events = tn_reactor_epoll_events(events), .data.fd = fd};
      if (epoll_ctl(reactor->backend, EPOLL_CTL_MOD, fd, &event) != 0) {
        int status = errno;
        pthread_mutex_unlock(&reactor->mutex);
        return status;
      }
#elif defined(__APPLE__)
      if (tn_reactor_kevent_update(reactor->backend, fd, reactor->registrations[index].events,
                                   EV_DELETE) != 0 ||
          tn_reactor_kevent_update(reactor->backend, fd, events, EV_ADD | EV_ENABLE) != 0) {
        int status = errno;
        pthread_mutex_unlock(&reactor->mutex);
        return status;
      }
#endif
      reactor->registrations[index].events = events;
      pthread_mutex_unlock(&reactor->mutex);
      return 0;
    }
  }
  if (reactor->length == reactor->capacity) {
    if (reactor->capacity > SIZE_MAX / 2) {
      pthread_mutex_unlock(&reactor->mutex);
      return ENOMEM;
    }
    size_t capacity = reactor->capacity == 0 ? 8 : reactor->capacity * 2;
    tn_reactor_registration *registrations = realloc(
        reactor->registrations, capacity * sizeof(*registrations));
    if (registrations == NULL) {
      pthread_mutex_unlock(&reactor->mutex);
      return ENOMEM;
    }
    reactor->registrations = registrations;
    reactor->capacity = capacity;
  }
#if defined(__linux__)
  struct epoll_event event = {.events = tn_reactor_epoll_events(events), .data.fd = fd};
  if (epoll_ctl(reactor->backend, EPOLL_CTL_ADD, fd, &event) != 0) {
    int status = errno;
    pthread_mutex_unlock(&reactor->mutex);
    return status;
  }
#elif defined(__APPLE__)
  if (tn_reactor_kevent_update(reactor->backend, fd, events, EV_ADD | EV_ENABLE) != 0) {
    int status = errno;
    pthread_mutex_unlock(&reactor->mutex);
    return status;
  }
#endif
  reactor->registrations[reactor->length++] = (tn_reactor_registration){.handle = fd,
                                                                         .events = events};
  pthread_mutex_unlock(&reactor->mutex);
  return 0;
}

int tn_reactor_unwatch(void *handle, int fd) {
  tn_reactor *reactor = handle;
  if (reactor == NULL) {
    return EINVAL;
  }
  pthread_mutex_lock(&reactor->mutex);
  for (size_t index = 0; index < reactor->length; ++index) {
    if (reactor->registrations[index].handle == fd) {
#if defined(__linux__)
      if (epoll_ctl(reactor->backend, EPOLL_CTL_DEL, fd, NULL) != 0 && errno != ENOENT) {
        int status = errno;
        pthread_mutex_unlock(&reactor->mutex);
        return status;
      }
#elif defined(__APPLE__)
      if (tn_reactor_kevent_update(reactor->backend, fd, reactor->registrations[index].events,
                                   EV_DELETE) != 0 && errno != ENOENT) {
        int status = errno;
        pthread_mutex_unlock(&reactor->mutex);
        return status;
      }
#endif
      reactor->registrations[index] = reactor->registrations[--reactor->length];
      pthread_mutex_unlock(&reactor->mutex);
      return 0;
    }
  }
  pthread_mutex_unlock(&reactor->mutex);
  return ENOENT;
}

int tn_reactor_wait(void *handle, int timeout_ms, int *ready_fd, short *ready_events) {
  tn_reactor *reactor = handle;
  if (reactor == NULL || ready_fd == NULL || ready_events == NULL) {
    return EINVAL;
  }
#if defined(__linux__)
  struct epoll_event event;
  int result = epoll_wait(reactor->backend, &event, 1, timeout_ms);
  if (result > 0) {
    *ready_fd = event.data.fd;
    *ready_events = 0;
    if ((event.events & (EPOLLIN | EPOLLPRI)) != 0) {
      *ready_events |= POLLIN;
    }
    if ((event.events & EPOLLOUT) != 0) {
      *ready_events |= POLLOUT;
    }
    if ((event.events & EPOLLERR) != 0) {
      *ready_events |= POLLERR;
    }
    if ((event.events & EPOLLHUP) != 0) {
      *ready_events |= POLLHUP;
    }
  }
  return result < 0 ? -errno : result;
#elif defined(__APPLE__)
  struct kevent event;
  struct timespec timeout = {
      .tv_sec = timeout_ms < 0 ? 0 : timeout_ms / 1000,
      .tv_nsec = timeout_ms < 0 ? 0 : (long)(timeout_ms % 1000) * 1000000L,
  };
  int result = kevent(reactor->backend, NULL, 0, &event, 1, timeout_ms < 0 ? NULL : &timeout);
  if (result > 0) {
    *ready_fd = (int)event.ident;
    *ready_events = event.filter == EVFILT_WRITE ? POLLOUT : POLLIN;
    if ((event.flags & EV_EOF) != 0) {
      *ready_events |= POLLHUP;
    }
    if ((event.flags & EV_ERROR) != 0) {
      *ready_events |= POLLERR;
    }
  }
  return result < 0 ? -errno : result;
#else
  pthread_mutex_lock(&reactor->mutex);
  size_t length = reactor->length;
  struct pollfd *polls = calloc(length == 0 ? 1 : length, sizeof(*polls));
  if (polls == NULL) {
    pthread_mutex_unlock(&reactor->mutex);
    return ENOMEM;
  }
  for (size_t index = 0; index < length; ++index) {
    polls[index].fd = reactor->registrations[index].handle;
    polls[index].events = reactor->registrations[index].events;
  }
  pthread_mutex_unlock(&reactor->mutex);
  int result = poll(polls, length, timeout_ms);
  if (result > 0) {
    for (size_t index = 0; index < length; ++index) {
      if (polls[index].revents != 0) {
        *ready_fd = polls[index].fd;
        *ready_events = polls[index].revents;
        break;
      }
    }
  }
  free(polls);
  return result < 0 ? -errno : result;
#endif
}

int tn_reactor_destroy(void *handle) {
  tn_reactor *reactor = handle;
  if (reactor == NULL) {
    return 0;
  }
  int status = pthread_mutex_destroy(&reactor->mutex);
  if (status == 0) {
#if defined(__linux__) || defined(__APPLE__)
    close(reactor->backend);
#endif
    free(reactor->registrations);
    free(reactor);
  }
  return status;
}

int tn_utf8_validate(const uint8_t *bytes, size_t length) {
  size_t index = 0;
  while (index < length) {
    uint8_t first = bytes[index++];
    size_t continuation = 0;
    uint32_t value = 0;
    if (first < 0x80) {
      continue;
    }
    if (first >= 0xc2 && first <= 0xdf) {
      continuation = 1;
      value = first & 0x1f;
    } else if (first >= 0xe0 && first <= 0xef) {
      continuation = 2;
      value = first & 0x0f;
    } else if (first >= 0xf0 && first <= 0xf4) {
      continuation = 3;
      value = first & 0x07;
    } else {
      return 0;
    }
    if (continuation > length - index) {
      return 0;
    }
    for (size_t offset = 0; offset < continuation; ++offset) {
      uint8_t next = bytes[index++];
      if ((next & 0xc0) != 0x80) {
        return 0;
      }
      value = (value << 6) | (next & 0x3f);
    }
    if (value > 0x10ffff || (value >= 0xd800 && value <= 0xdfff) ||
        (continuation == 2 && value < 0x800) || (continuation == 3 && value < 0x10000)) {
      return 0;
    }
  }
  return 1;
}

char *tn_string_from_bytes(const uint8_t *bytes, size_t length) {
  if (bytes == NULL && length != 0) {
    return NULL;
  }
  if (length == SIZE_MAX) {
    return NULL;
  }
  char *result = tn_runtime_alloc(length + 1);
  if (length != 0) {
    memcpy(result, bytes, length);
  }
  result[length] = '\0';
  return result;
}

int tn_string_equals(const uint8_t *left, const uint8_t *right) {
  if (left == NULL || right == NULL) {
    return left == right;
  }
  return strcmp((const char *)left, (const char *)right) == 0;
}

char *tn_string_to_ascii_upper(const uint8_t *text) {
  if (text == NULL) {
    return NULL;
  }
  size_t length = strlen((const char *)text);
  char *result = tn_runtime_alloc(length + 1);
  for (size_t index = 0; index < length; ++index) {
    uint8_t value = text[index];
    result[index] = (char)(value >= 'a' && value <= 'z' ? value - ('a' - 'A') : value);
  }
  result[length] = '\0';
  tn_runtime_free((void *)text);
  return result;
}

int tn_utf8_decode(const uint8_t *bytes, size_t length, size_t offset, uint32_t *codepoint,
                   size_t *next_offset) {
  if (bytes == NULL || codepoint == NULL || next_offset == NULL || offset >= length) {
    return 0;
  }
  uint8_t first = bytes[offset++];
  size_t continuation = 0;
  uint32_t value = 0;
  if (first < 0x80) {
    value = first;
  } else if (first >= 0xc2 && first <= 0xdf) {
    continuation = 1;
    value = first & 0x1f;
  } else if (first >= 0xe0 && first <= 0xef) {
    continuation = 2;
    value = first & 0x0f;
  } else if (first >= 0xf0 && first <= 0xf4) {
    continuation = 3;
    value = first & 0x07;
  } else {
    return -EINVAL;
  }
  if (continuation > length - offset) {
    return -EINVAL;
  }
  for (size_t index = 0; index < continuation; ++index) {
    uint8_t next = bytes[offset++];
    if ((next & 0xc0) != 0x80) {
      return -EINVAL;
    }
    value = (value << 6) | (next & 0x3f);
  }
  if (value > 0x10ffff || (value >= 0xd800 && value <= 0xdfff) ||
      (continuation == 2 && value < 0x800) || (continuation == 3 && value < 0x10000)) {
    return -EINVAL;
  }
  *codepoint = value;
  *next_offset = offset;
  return 1;
}

int tn_utf8_slice_copy(const uint8_t *bytes, size_t length, size_t start, size_t end,
                       uint8_t *destination, size_t capacity, size_t *written) {
  if (bytes == NULL || start > end || end > length || end - start == SIZE_MAX ||
      capacity < end - start + 1 ||
      (end - start != 0 && destination == NULL)) {
    return -EINVAL;
  }
  if (!tn_utf8_validate(bytes, length)) {
    return -EILSEQ;
  }
  for (size_t boundary = 0; boundary < length; ) {
    if (boundary == start || boundary == end) {
      if (boundary == end) {
        break;
      }
    }
    size_t next = boundary;
    uint32_t codepoint = 0;
    if (tn_utf8_decode(bytes, length, boundary, &codepoint, &next) <= 0) {
      return -EILSEQ;
    }
    if (boundary < start && next > start) {
      return -ERANGE;
    }
    if (boundary < end && next > end) {
      return -ERANGE;
    }
    boundary = next;
  }
  if (end - start != 0) {
    memcpy(destination, bytes + start, end - start);
  }
  if (destination != NULL) {
    destination[end - start] = '\0';
  }
  if (written != NULL) {
    *written = end - start;
  }
  return 0;
}

size_t tn_bytes_copy(const uint8_t *source, size_t length, uint8_t *destination) {
  if (source == NULL || destination == NULL) {
    return 0;
  }
  memcpy(destination, source, length);
  return length;
}

size_t tn_bytes_move(const uint8_t *source, size_t length, uint8_t *destination) {
  if (source == NULL || destination == NULL) {
    return 0;
  }
  memmove(destination, source, length);
  return length;
}

int tn_bytes_copy_at(const uint8_t *source, size_t length, size_t offset, size_t count,
                     uint8_t *destination) {
  if (source == NULL || destination == NULL || offset > length || count > length - offset) {
    return -EINVAL;
  }
  memcpy(destination, source + offset, count);
  return 0;
}

int tn_bytes_copy_to_at(const uint8_t *source, size_t source_length, size_t source_offset,
                        size_t count, uint8_t *destination, size_t destination_length,
                        size_t destination_offset) {
  if (source == NULL || destination == NULL || source_offset > source_length ||
      count > source_length - source_offset || destination_offset > destination_length ||
      count > destination_length - destination_offset) {
    return -EINVAL;
  }
  memcpy(destination + destination_offset, source + source_offset, count);
  return 0;
}

int tn_bytes_move_at(uint8_t *bytes, size_t length, size_t source_offset, size_t destination_offset,
                     size_t count) {
  if (bytes == NULL || source_offset > length || destination_offset > length ||
      count > length - source_offset || count > length - destination_offset) {
    return -EINVAL;
  }
  memmove(bytes + destination_offset, bytes + source_offset, count);
  return 0;
}

int tn_bytes_read_u8(const uint8_t *bytes, size_t length, size_t index, uint8_t *value) {
  if (bytes == NULL || value == NULL || index >= length) {
    return -EINVAL;
  }
  *value = bytes[index];
  return 0;
}

int tn_bytes_read_u8_mut(uint8_t *bytes, size_t length, size_t index, uint8_t *value) {
  return tn_bytes_read_u8(bytes, length, index, value);
}

int tn_bytes_write_u8(uint8_t *bytes, size_t length, size_t index, uint8_t value) {
  if (bytes == NULL || index >= length) {
    return -EINVAL;
  }
  bytes[index] = value;
  return 0;
}

const uint8_t *tn_bytes_slice(const uint8_t *bytes, size_t length, size_t start, size_t end) {
  if (bytes == NULL || start > end || end > length) {
    return NULL;
  }
  return bytes + start;
}

const uint8_t *tn_bytes_slice_mut(uint8_t *bytes, size_t length, size_t start, size_t end) {
  return tn_bytes_slice(bytes, length, start, end);
}

size_t tn_bytes_append_ascii(uint8_t *buffer, size_t capacity, size_t length,
                             const uint8_t *text, size_t text_length) {
  if ((buffer == NULL && capacity != 0) || (text == NULL && text_length != 0) ||
      text_length > capacity - (length <= capacity ? length : capacity)) {
    return SIZE_MAX;
  }
  if (text_length != 0) {
    memcpy(buffer + length, text, text_length);
  }
  return length + text_length;
}

size_t tn_bytes_append_safe_ascii(uint8_t *buffer, size_t capacity, size_t length,
                                  const uint8_t *text, size_t text_length) {
  if ((buffer == NULL && capacity != 0) || (text == NULL && text_length != 0) ||
      text_length > capacity - (length <= capacity ? length : capacity)) {
    return SIZE_MAX;
  }
  for (size_t index = 0; index < text_length; ++index) {
    uint8_t value = text[index];
    buffer[length + index] = value == '\r' || value == '\n' ? ' ' : value;
  }
  return length + text_length;
}

size_t tn_bytes_append_bytes(uint8_t *buffer, size_t capacity, size_t length,
                             const uint8_t *source, size_t source_length) {
  return tn_bytes_append_ascii(buffer, capacity, length, source, source_length);
}

size_t tn_bytes_append_decimal(uint8_t *buffer, size_t capacity, size_t length,
                               intptr_t value) {
  if (buffer == NULL || length > capacity) {
    return SIZE_MAX;
  }
  char rendered[3 * sizeof(intptr_t) + 4];
  int written = snprintf(rendered, sizeof(rendered), "%" PRIdPTR, value);
  if (written < 0 || (size_t)written > capacity - length) {
    return SIZE_MAX;
  }
  memcpy(buffer + length, rendered, (size_t)written);
  return length + (size_t)written;
}

size_t tn_bytes_append_usize(uint8_t *buffer, size_t capacity, size_t length,
                             size_t value) {
  if (buffer == NULL || length > capacity) {
    return SIZE_MAX;
  }
  char rendered[3 * sizeof(size_t) + 4];
  int written = snprintf(rendered, sizeof(rendered), "%" PRIuPTR, (uintptr_t)value);
  if (written < 0 || (size_t)written > capacity - length) {
    return SIZE_MAX;
  }
  memcpy(buffer + length, rendered, (size_t)written);
  return length + (size_t)written;
}

int tn_parse_ascii(const uint8_t *bytes, size_t length, size_t *value, size_t *offset) {
  if (bytes == NULL || value == NULL || offset == NULL || length == 0) {
    if (offset != NULL) {
      *offset = 0;
    }
    return EINVAL;
  }
  size_t parsed = 0;
  for (size_t index = 0; index < length; ++index) {
    if (bytes[index] < '0' || bytes[index] > '9') {
      *offset = index;
      return EINVAL;
    }
    size_t digit = (size_t)(bytes[index] - '0');
    if (parsed > (SIZE_MAX - digit) / 10) {
      *offset = index;
      return ERANGE;
    }
    parsed = parsed * 10 + digit;
  }
  *value = parsed;
  *offset = length;
  return 0;
}

typedef struct {
  uint8_t *key;
  uint8_t *value;
  size_t key_length;
  size_t value_length;
  int occupied;
} tn_map_entry;

typedef struct {
  size_t key_size;
  size_t value_size;
  size_t length;
  size_t capacity;
  int ordered;
  int string_keys;
  int string_values;
  tn_map_entry *entries;
  pthread_mutex_t mutex;
} tn_map;

static uint64_t tn_map_hash(const uint8_t *bytes, size_t length) {
  uint64_t hash = UINT64_C(1469598103934665603);
  for (size_t index = 0; index < length; ++index) {
    hash ^= bytes[index];
    hash *= UINT64_C(1099511628211);
  }
  return hash;
}

static int tn_map_entry_matches(const tn_map *map, const tn_map_entry *entry,
                                const uint8_t *key) {
  if (!entry->occupied || key == NULL) {
    return 0;
  }
  if (map->string_keys) {
    const char *incoming = *(const char *const *)key;
    return incoming != NULL && entry->key != NULL &&
           strcmp((const char *)entry->key, incoming) == 0;
  }
  return map->key_size != 0 && memcmp(entry->key, key, map->key_size) == 0;
}

static uint64_t tn_map_key_hash(const tn_map *map, const uint8_t *key) {
  if (map->string_keys) {
    const char *incoming = *(const char *const *)key;
    return tn_map_hash((const uint8_t *)incoming, strlen(incoming));
  }
  return tn_map_hash(key, map->key_size);
}

static uint64_t tn_map_entry_hash(const tn_map_entry *entry) {
  return tn_map_hash(entry->key, entry->key_length);
}

static int tn_map_copy_payload(const void *source, size_t size, int string_value,
                               uint8_t **destination, size_t *length) {
  if (string_value) {
    const char *text = *(const char *const *)source;
    if (text == NULL) {
      return EINVAL;
    }
    size_t text_length = strlen(text);
    if (text_length == SIZE_MAX) {
      return EOVERFLOW;
    }
    uint8_t *copy = tn_runtime_try_alloc(text_length + 1);
    if (copy == NULL) {
      return ENOMEM;
    }
    memcpy(copy, text, text_length + 1);
    *destination = copy;
    *length = text_length;
    return 0;
  }
  if (size == 0) {
    *destination = NULL;
    *length = 0;
    return 0;
  }
  uint8_t *copy = tn_runtime_try_alloc(size);
  if (copy == NULL) {
    return ENOMEM;
  }
  memcpy(copy, source, size);
  *destination = copy;
  *length = size;
  return 0;
}

static int tn_map_copy_stored(const tn_map_entry *entry, int string_value,
                              void *destination) {
  if (string_value) {
    uint8_t *copy = tn_runtime_try_alloc(entry->value_length + 1);
    if (copy == NULL) {
      return ENOMEM;
    }
    memcpy(copy, entry->value, entry->value_length + 1);
    *(uint8_t **)destination = copy;
    return 0;
  }
  if (entry->value_length != 0) {
    memcpy(destination, entry->value, entry->value_length);
  }
  return 0;
}

static void tn_map_free_entry(tn_map_entry *entry) {
  tn_runtime_free(entry->key);
  tn_runtime_free(entry->value);
  memset(entry, 0, sizeof(*entry));
}

static size_t tn_map_ordered_position(const tn_map *map, const uint8_t *key,
                                      int *found) {
  size_t position = 0;
  *found = 0;
  while (position < map->length) {
    int comparison = map->string_keys
                         ? strcmp((const char *)map->entries[position].key,
                                  *(const char *const *)key)
                         : memcmp(map->entries[position].key, key, map->key_size);
    if (comparison == 0) {
      *found = 1;
      break;
    }
    if (comparison > 0) {
      break;
    }
    ++position;
  }
  return position;
}

static int tn_map_resize(tn_map *map, size_t capacity) {
  if (capacity < 8 || capacity > SIZE_MAX / sizeof(tn_map_entry)) {
    return ENOMEM;
  }
  tn_map_entry *entries = tn_runtime_try_alloc(capacity * sizeof(*entries));
  if (entries == NULL) {
    return ENOMEM;
  }
  if (map->ordered) {
    memcpy(entries, map->entries, map->length * sizeof(*entries));
    tn_runtime_free(map->entries);
    map->entries = entries;
    map->capacity = capacity;
    return 0;
  }
  for (size_t index = 0; index < map->capacity; ++index) {
    tn_map_entry *old = &map->entries[index];
    if (!old->occupied) {
      continue;
    }
    size_t slot = (size_t)(tn_map_entry_hash(old) % capacity);
    while (entries[slot].occupied) {
      slot = (slot + 1) % capacity;
    }
    entries[slot] = *old;
  }
  tn_runtime_free(map->entries);
  map->entries = entries;
  map->capacity = capacity;
  return 0;
}

void *tn_map_create_ex(size_t key_size, size_t value_size, int ordered,
                       int string_keys, int string_values,
                       size_t initial_capacity) {
  if (key_size == 0 || value_size > SIZE_MAX - key_size) {
    return NULL;
  }
  tn_map *map = tn_runtime_try_alloc(sizeof(*map));
  if (map == NULL) {
    return NULL;
  }
  memset(map, 0, sizeof(*map));
  if (pthread_mutex_init(&map->mutex, NULL) != 0) {
    tn_runtime_free(map);
    return NULL;
  }
  map->key_size = key_size;
  map->value_size = value_size;
  map->ordered = ordered != 0;
  map->string_keys = string_keys != 0;
  map->string_values = string_values != 0;
  map->capacity = initial_capacity < 8 ? 8 : initial_capacity;
  if (map->capacity > SIZE_MAX / sizeof(*map->entries)) {
    pthread_mutex_destroy(&map->mutex);
    tn_runtime_free(map);
    return NULL;
  }
  map->entries = tn_runtime_try_alloc(map->capacity * sizeof(*map->entries));
  if (map->entries == NULL) {
    pthread_mutex_destroy(&map->mutex);
    tn_runtime_free(map);
    return NULL;
  }
  return map;
}

void *tn_map_create(size_t key_size, size_t value_size, int ordered) {
  return tn_map_create_ex(key_size, value_size, ordered, 0, 0, 16);
}

int tn_map_insert(void *handle, const void *key, const void *value) {
  tn_map *map = handle;
  if (map == NULL || key == NULL || (map->value_size != 0 && value == NULL)) {
    return -EINVAL;
  }
  if (map->string_keys && *(const char *const *)key == NULL) {
    return -EINVAL;
  }
  if (map->string_values && *(const char *const *)value == NULL) {
    return -EINVAL;
  }
  pthread_mutex_lock(&map->mutex);
  if (map->ordered) {
    int found = 0;
    size_t position = tn_map_ordered_position(map, key, &found);
    if (found) {
      if (map->value_size != 0) {
        uint8_t *new_value = NULL;
        size_t new_length = 0;
        int status = tn_map_copy_payload(value, map->value_size,
                                         map->string_values, &new_value,
                                         &new_length);
        if (status != 0) {
          pthread_mutex_unlock(&map->mutex);
          return -status;
        }
        tn_runtime_free(map->entries[position].value);
        map->entries[position].value = new_value;
        map->entries[position].value_length = new_length;
      }
      pthread_mutex_unlock(&map->mutex);
      return 0;
    }
    if (map->length == map->capacity) {
      size_t capacity = map->capacity <= SIZE_MAX / 2 ? map->capacity * 2 : 0;
      int status = capacity == 0 ? ENOMEM : tn_map_resize(map, capacity);
      if (status != 0) {
        pthread_mutex_unlock(&map->mutex);
        return -status;
      }
    }
    uint8_t *new_key = NULL;
    size_t new_key_length = 0;
    int key_status = tn_map_copy_payload(key, map->key_size, map->string_keys,
                                         &new_key, &new_key_length);
    if (key_status != 0) {
      pthread_mutex_unlock(&map->mutex);
      return -key_status;
    }
    uint8_t *new_value = NULL;
    size_t new_value_length = 0;
    if (map->value_size != 0) {
      int value_status = tn_map_copy_payload(value, map->value_size,
                                             map->string_values, &new_value,
                                             &new_value_length);
      if (value_status != 0) {
        tn_runtime_free(new_key);
        pthread_mutex_unlock(&map->mutex);
        return -value_status;
      }
    }
    for (size_t index = map->length; index > position; --index) {
      map->entries[index] = map->entries[index - 1];
    }
    tn_map_entry *entry = &map->entries[position];
    entry->key = new_key;
    entry->value = new_value;
    entry->key_length = new_key_length;
    entry->value_length = new_value_length;
    entry->occupied = 1;
    ++map->length;
    pthread_mutex_unlock(&map->mutex);
    return 0;
  }
  if (map->length >= map->capacity - (map->capacity / 3) &&
      map->capacity <= SIZE_MAX / 2) {
    int status = tn_map_resize(map, map->capacity * 2);
    if (status != 0) {
      pthread_mutex_unlock(&map->mutex);
      return -status;
    }
  }
  size_t slot = (size_t)(tn_map_key_hash(map, key) % map->capacity);
  while (map->entries[slot].occupied &&
         !tn_map_entry_matches(map, &map->entries[slot], key)) {
    slot = (slot + 1) % map->capacity;
  }
  tn_map_entry *entry = &map->entries[slot];
  if (!entry->occupied) {
    size_t key_length = 0;
    int key_status = tn_map_copy_payload(key, map->key_size, map->string_keys,
                                         &entry->key, &key_length);
    if (key_status != 0) {
      pthread_mutex_unlock(&map->mutex);
      return -key_status;
    }
    entry->key_length = key_length;
    if (map->value_size != 0) {
      int value_status = tn_map_copy_payload(
          value, map->value_size, map->string_values, &entry->value,
          &entry->value_length);
      if (value_status != 0) {
        tn_runtime_free(entry->key);
        memset(entry, 0, sizeof(*entry));
        pthread_mutex_unlock(&map->mutex);
        return -value_status;
      }
    }
    entry->occupied = 1;
    ++map->length;
  } else if (map->value_size != 0) {
    uint8_t *new_value = NULL;
    size_t new_length = 0;
    int status = tn_map_copy_payload(value, map->value_size,
                                     map->string_values, &new_value,
                                     &new_length);
    if (status != 0) {
      pthread_mutex_unlock(&map->mutex);
      return -status;
    }
    tn_runtime_free(entry->value);
    entry->value = new_value;
    entry->value_length = new_length;
  }
  pthread_mutex_unlock(&map->mutex);
  return 0;
}

int tn_map_get(void *handle, const void *key, void *value) {
  tn_map *map = handle;
  if (map == NULL || key == NULL || (map->value_size != 0 && value == NULL)) {
    return -EINVAL;
  }
  if (map->string_keys && *(const char *const *)key == NULL) {
    return -EINVAL;
  }
  pthread_mutex_lock(&map->mutex);
  if (map->ordered) {
    int found = 0;
    size_t position = tn_map_ordered_position(map, key, &found);
    if (found && map->value_size != 0) {
        int status = tn_map_copy_stored(&map->entries[position],
                                        map->string_values, value);
        if (status != 0) {
          pthread_mutex_unlock(&map->mutex);
          return -status;
        }
    }
    pthread_mutex_unlock(&map->mutex);
    return found;
  }
  size_t slot = (size_t)(tn_map_key_hash(map, key) % map->capacity);
  size_t scanned = 0;
  while (scanned++ < map->capacity) {
    tn_map_entry *entry = &map->entries[slot];
    if (!entry->occupied) {
      pthread_mutex_unlock(&map->mutex);
      return 0;
    }
    if (tn_map_entry_matches(map, entry, key)) {
      if (map->value_size != 0) {
        int status = tn_map_copy_stored(entry, map->string_values, value);
        if (status != 0) {
          pthread_mutex_unlock(&map->mutex);
          return -status;
        }
      }
      pthread_mutex_unlock(&map->mutex);
      return 1;
    }
    slot = (slot + 1) % map->capacity;
  }
  pthread_mutex_unlock(&map->mutex);
  return 0;
}

int tn_map_contains(void *handle, const void *key) {
  tn_map *map = handle;
  if (map == NULL || key == NULL) {
    return -EINVAL;
  }
  if (map->string_keys && *(const char *const *)key == NULL) {
    return -EINVAL;
  }
  pthread_mutex_lock(&map->mutex);
  if (map->ordered) {
    int found = 0;
    (void)tn_map_ordered_position(map, key, &found);
    pthread_mutex_unlock(&map->mutex);
    return found;
  }
  size_t slot = (size_t)(tn_map_key_hash(map, key) % map->capacity);
  size_t scanned = 0;
  while (scanned++ < map->capacity) {
    tn_map_entry *entry = &map->entries[slot];
    if (!entry->occupied) {
      pthread_mutex_unlock(&map->mutex);
      return 0;
    }
    if (tn_map_entry_matches(map, entry, key)) {
      pthread_mutex_unlock(&map->mutex);
      return 1;
    }
    slot = (slot + 1) % map->capacity;
  }
  pthread_mutex_unlock(&map->mutex);
  return 0;
}

int tn_map_remove(void *handle, const void *key) {
  tn_map *map = handle;
  if (map == NULL || key == NULL) {
    return -EINVAL;
  }
  if (map->string_keys && *(const char *const *)key == NULL) {
    return -EINVAL;
  }
  pthread_mutex_lock(&map->mutex);
  if (map->ordered) {
    int found = 0;
    size_t position = tn_map_ordered_position(map, key, &found);
    if (!found) {
      pthread_mutex_unlock(&map->mutex);
      return 0;
    }
    tn_map_free_entry(&map->entries[position]);
    for (size_t index = position + 1; index < map->length; ++index) {
      map->entries[index - 1] = map->entries[index];
    }
    memset(&map->entries[map->length - 1], 0, sizeof(map->entries[0]));
    --map->length;
    pthread_mutex_unlock(&map->mutex);
    return 1;
  }
  size_t slot = (size_t)(tn_map_key_hash(map, key) % map->capacity);
  size_t scanned = 0;
  while (scanned++ < map->capacity) {
    tn_map_entry *entry = &map->entries[slot];
    if (!entry->occupied) {
      pthread_mutex_unlock(&map->mutex);
      return 0;
    }
    if (tn_map_entry_matches(map, entry, key)) {
      tn_map_free_entry(entry);
      --map->length;

      /* Reinsert the remainder of this probe cluster so lookups do not stop
       * at the newly-created empty slot.  The key/value allocations are
       * retained; only their table slot changes. */
      size_t next = (slot + 1) % map->capacity;
      while (map->entries[next].occupied) {
        tn_map_entry displaced = map->entries[next];
        memset(&map->entries[next], 0, sizeof(map->entries[next]));
        --map->length;
        size_t target = map->ordered
                            ? 0
                            : (size_t)(tn_map_entry_hash(&displaced) %
                                       map->capacity);
        while (map->entries[target].occupied) {
          target = (target + 1) % map->capacity;
        }
        map->entries[target] = displaced;
        ++map->length;
        next = (next + 1) % map->capacity;
      }
      pthread_mutex_unlock(&map->mutex);
      return 1;
    }
    slot = (slot + 1) % map->capacity;
  }
  pthread_mutex_unlock(&map->mutex);
  return 0;
}

int tn_map_clear(void *handle) {
  tn_map *map = handle;
  if (map == NULL) {
    return -EINVAL;
  }
  pthread_mutex_lock(&map->mutex);
  for (size_t index = 0; index < map->capacity; ++index) {
    if (map->entries[index].occupied) {
      tn_map_free_entry(&map->entries[index]);
    }
  }
  map->length = 0;
  pthread_mutex_unlock(&map->mutex);
  return 0;
}

size_t tn_map_length(void *handle) {
  tn_map *map = handle;
  if (map == NULL) {
    return 0;
  }
  pthread_mutex_lock(&map->mutex);
  size_t length = map->length;
  pthread_mutex_unlock(&map->mutex);
  return length;
}

int tn_map_next(void *handle, size_t *cursor, void *key, void *value) {
  tn_map *map = handle;
  if (map == NULL || cursor == NULL || key == NULL ||
      (map->value_size != 0 && value == NULL)) {
    return -EINVAL;
  }
  pthread_mutex_lock(&map->mutex);
  size_t index = *cursor;
  while (index < map->capacity && !map->entries[index].occupied) {
    ++index;
  }
  if (index == map->capacity) {
    *cursor = index;
    pthread_mutex_unlock(&map->mutex);
    return 0;
  }
  if (map->string_keys) {
    uint8_t *copy = tn_runtime_try_alloc(map->entries[index].key_length + 1);
    if (copy == NULL) {
      pthread_mutex_unlock(&map->mutex);
      return -ENOMEM;
    }
    memcpy(copy, map->entries[index].key, map->entries[index].key_length + 1);
    *(uint8_t **)key = copy;
  } else {
    memcpy(key, map->entries[index].key, map->key_size);
  }
  if (map->value_size != 0) {
    int status = tn_map_copy_stored(&map->entries[index], map->string_values, value);
    if (status != 0) {
      if (map->string_keys) {
        tn_runtime_free(*(uint8_t **)key);
      }
      pthread_mutex_unlock(&map->mutex);
      return -status;
    }
  }
  *cursor = index + 1;
  pthread_mutex_unlock(&map->mutex);
  return 1;
}

int tn_map_destroy(void *handle) {
  tn_map *map = handle;
  if (map == NULL) {
    return 0;
  }
  pthread_mutex_lock(&map->mutex);
  for (size_t index = 0; index < map->capacity; ++index) {
    if (map->entries[index].occupied) {
      tn_runtime_free(map->entries[index].key);
      tn_runtime_free(map->entries[index].value);
    }
  }
  tn_runtime_free(map->entries);
  pthread_mutex_unlock(&map->mutex);
  pthread_mutex_destroy(&map->mutex);
  tn_runtime_free(map);
  return 0;
}

int tn_cstring_length(const uint8_t *bytes, size_t length, size_t *first_nul) {
  if (bytes == NULL) {
    return -EINVAL;
  }
  for (size_t index = 0; index < length; ++index) {
    if (bytes[index] == 0) {
      if (first_nul != NULL) {
        *first_nul = index;
      }
      return -EINVAL;
    }
  }
  if (first_nul != NULL) {
    *first_nul = length;
  }
  return 0;
}

int tn_cstring_validate(const uint8_t *bytes, size_t length) {
  return tn_cstring_length(bytes, length, NULL);
}

char *tn_cstring_alloc(const uint8_t *bytes, size_t length) {
  if (length == SIZE_MAX || tn_cstring_length(bytes, length, NULL) != 0) {
    return NULL;
  }
  char *copy = malloc(length + 1);
  if (copy == NULL) {
    return NULL;
  }
  memcpy(copy, bytes, length);
  copy[length] = '\0';
  return copy;
}

int32_t tn_selfhost_token_is(const char *source, size_t start, size_t end,
                             const char *word) {
  if (source == NULL || word == NULL || end < start) {
    return 0;
  }
  size_t word_length = strlen(word);
  return word_length == end - start && memcmp(source + start, word, word_length) == 0;
}

int32_t tn_selfhost_cstring_equal(const uint8_t *left, const uint8_t *right) {
  if (left == NULL || right == NULL) {
    return left == right;
  }
  size_t left_length = strlen((const char *)left);
  size_t right_length = strlen((const char *)right);
  return left_length == right_length && memcmp(left, right, left_length) == 0;
}

int32_t tn_selfhost_path_is_directory(const char *path) {
  if (path == NULL) {
    return -EINVAL;
  }
  struct stat metadata;
  if (stat(path, &metadata) != 0) {
    return -errno;
  }
  return S_ISDIR(metadata.st_mode) ? 1 : 0;
}

int32_t tn_selfhost_path_has_tn_suffix(const char *path) {
  if (path == NULL) {
    return 0;
  }
  size_t length = strlen(path);
  return length >= 3 && memcmp(path + length - 3, ".tn", 3) == 0;
}

int tn_selfhost_write_diagnostic(int fd, int32_t code, size_t start, size_t end,
                                 int32_t json) {
  char rendered[256];
  int written;
  const char *condition = code == 1015
                              ? "SYNTAX_INVALID_INTEGER_SUFFIX"
                              : code >= 2000 ? "SEMANTIC_SELFHOST" : "SYNTAX_SELFHOST";
  if (json && code == 1015) {
    written = snprintf(rendered, sizeof(rendered),
                       "{\"condition\":\"SYNTAX_INVALID_INTEGER_SUFFIX\",\"severity\":\"error\","
                       "\"message\":\"invalid integer literal suffix\",\"primary\":{\"byte_start\":%zu,"
                       "\"byte_end\":%zu}}\n",
                       start, end);
  } else if (json) {
    written = snprintf(rendered, sizeof(rendered),
                       "{\"condition\":\"%s\",\"code\":%" PRId32
                       ",\"severity\":\"error\",\"primary\":{\"byte_start\":%zu,\"byte_end\":%zu}}\n",
                       condition, code, start, end);
  } else {
    written = snprintf(rendered, sizeof(rendered),
                       "error[%s](%" PRId32 ") at bytes %zu..%zu\n", condition, code, start,
                       end);
  }
  if (written < 0 || (size_t)written >= sizeof(rendered)) {
    return -EOVERFLOW;
  }
  return tn_io_write_all(fd, rendered, (size_t)written);
}

int tn_selfhost_write_timing(const char *phase, uint64_t nanoseconds) {
  if (phase == NULL) {
    return -EINVAL;
  }
  char rendered[256];
  int written = snprintf(rendered, sizeof(rendered), "tn-timing phase=%s nanos=%" PRIu64 "\n",
                         phase, nanoseconds);
  if (written < 0 || (size_t)written >= sizeof(rendered)) {
    return -EOVERFLOW;
  }
  return tn_io_write_all(STDERR_FILENO, rendered, (size_t)written);
}

int tn_selfhost_write_timing_usize(const char *phase, size_t value) {
  return tn_selfhost_write_timing(phase, (uint64_t)value);
}


int tn_selfhost_write(const char *output_path, const uint8_t *source, size_t length) {
  if (output_path == NULL || (source == NULL && length != 0)) {
    return -EINVAL;
  }
  if (length > UINTMAX_C(64) * 1024 * 1024) {
    return -EFBIG;
  }
  int output = open(output_path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (output < 0) {
    return -errno;
  }
  int status = length == 0 ? 0 : tn_io_write_all(output, source, length);
  if (close(output) != 0 && status == 0) {
    status = -errno;
  }
  return status;
}

int tn_selfhost_read(const char *input_path, uint8_t **output_source, size_t *output_length) {
  if (input_path == NULL || output_source == NULL || output_length == NULL) {
    return -EINVAL;
  }
  *output_source = NULL;
  *output_length = 0;
  int input = open(input_path, O_RDONLY);
  if (input < 0) {
    return -errno;
  }
  struct stat metadata;
  if (fstat(input, &metadata) != 0) {
    int status = -errno;
    close(input);
    return status;
  }
  if (metadata.st_size < 0 || (uintmax_t)metadata.st_size > UINTMAX_C(64) * 1024 * 1024) {
    close(input);
    return -EFBIG;
  }
  size_t length = (size_t)metadata.st_size;
  char *source = malloc(length + 1);
  if (source == NULL) {
    close(input);
    return -ENOMEM;
  }
  size_t offset = 0;
  while (offset < length) {
    ssize_t received = read(input, source + offset, length - offset);
    if (received < 0 && errno == EINTR) {
      continue;
    }
    if (received <= 0) {
      int status = received == 0 ? -EIO : -errno;
      free(source);
      close(input);
      return status;
    }
    offset += (size_t)received;
  }
  source[length] = '\0';
  close(input);
  if (!tn_utf8_validate((const uint8_t *)source, length)) {
    free(source);
    return -EILSEQ;
  }
  *output_source = (uint8_t *)source;
  *output_length = length;
  return 0;
}

void *tn_selfhost_null_pointer(void) { return NULL; }

void tn_selfhost_free(uint8_t *source) { free(source); }

static int tn_selfhost_project_entry_path(const char *input, char *output, size_t capacity) {
  if (input == NULL || output == NULL || capacity == 0) {
    return -EINVAL;
  }
  int directory = tn_selfhost_path_is_directory(input);
  if (directory < 0) {
    return directory;
  }
  if (directory == 0) {
    size_t input_length = strlen(input);
    if (input_length == SIZE_MAX || input_length + 1 > capacity) {
      return -ENAMETOOLONG;
    }
    memcpy(output, input, input_length + 1);
    return 0;
  }

  char configuration[PATH_MAX];
  int status = tn_path_join(input, "typenative.json", configuration, sizeof(configuration));
  if (status != 0) {
    return status;
  }
  uint8_t *source = NULL;
  size_t length = 0;
  status = tn_selfhost_read(configuration, &source, &length);
  if (status != 0) {
    return status;
  }

  static const char entry_key[] = "\"entry\"";
  size_t entry_start = SIZE_MAX;
  size_t entry_end = SIZE_MAX;
  for (size_t offset = 0; offset + sizeof(entry_key) - 1 <= length; ++offset) {
    if (memcmp(source + offset, entry_key, sizeof(entry_key) - 1) != 0) {
      continue;
    }
    size_t cursor = offset + sizeof(entry_key) - 1;
    while (cursor < length && isspace((unsigned char)source[cursor]) != 0) {
      ++cursor;
    }
    if (cursor >= length || source[cursor] != ':') {
      continue;
    }
    ++cursor;
    while (cursor < length && isspace((unsigned char)source[cursor]) != 0) {
      ++cursor;
    }
    if (cursor >= length || source[cursor] != '"') {
      continue;
    }
    entry_start = ++cursor;
    while (cursor < length && source[cursor] != '"') {
      if (source[cursor] == '\\') {
        free(source);
        return -EINVAL;
      }
      ++cursor;
    }
    if (cursor >= length) {
      free(source);
      return -EINVAL;
    }
    entry_end = cursor;
    break;
  }
  if (entry_start == SIZE_MAX || entry_end < entry_start || entry_end == entry_start) {
    free(source);
    return -EINVAL;
  }
  size_t entry_length = entry_end - entry_start;
  if (entry_length == SIZE_MAX || entry_length + 1 > PATH_MAX) {
    free(source);
    return -ENAMETOOLONG;
  }
  char entry[PATH_MAX];
  memcpy(entry, source + entry_start, entry_length);
  entry[entry_length] = '\0';
  free(source);
  return tn_path_join(input, entry, output, capacity);
}

int tn_selfhost_spans_equal(const uint8_t *source, size_t length, size_t left_start,
                            size_t left_end, size_t right_start, size_t right_end) {
  if (source == NULL || left_start > left_end || right_start > right_end || left_end > length ||
      right_end > length || left_end - left_start != right_end - right_start) {
    return 0;
  }
  return memcmp(source + left_start, source + right_start, left_end - left_start) == 0;
}

int tn_selfhost_load(const char *input_path, void *output) {
  if (output == NULL) {
    return -EINVAL;
  }
  struct tn_selfhost_buffer {
    uint8_t *source;
    size_t length;
  };
  struct tn_selfhost_buffer *buffer = output;
  char resolved[PATH_MAX];
  int status = tn_selfhost_project_entry_path(input_path, resolved, sizeof(resolved));
  if (status != 0) {
    return status;
  }
  return tn_selfhost_read(resolved, &buffer->source, &buffer->length);
}

static int tn_selfhost_identifier_start(uint8_t value) {
  return (value >= 'A' && value <= 'Z') || (value >= 'a' && value <= 'z') || value == '_' ||
         value >= 0x80;
}

static int tn_selfhost_identifier_continue(uint8_t value) {
  return tn_selfhost_identifier_start(value) || (value >= '0' && value <= '9');
}

static int tn_selfhost_word_at(const uint8_t *source, size_t length, size_t offset,
                               const char *word) {
  size_t word_length = strlen(word);
  if (offset + word_length > length || memcmp(source + offset, word, word_length) != 0) {
    return 0;
  }
  if (offset > 0 && tn_selfhost_identifier_continue(source[offset - 1])) {
    return 0;
  }
  if (offset + word_length < length && tn_selfhost_identifier_continue(source[offset + word_length])) {
    return 0;
  }
  return 1;
}

static size_t tn_selfhost_skip_space(const uint8_t *source, size_t length, size_t offset) {
  while (offset < length && isspace((unsigned char)source[offset])) {
    offset += 1;
  }
  return offset;
}

static int tn_selfhost_exported_name(const uint8_t *source, size_t length, const uint8_t *name,
                                     size_t name_length) {
  for (size_t offset = 0; offset < length; ++offset) {
    if (!tn_selfhost_word_at(source, length, offset, "export")) {
      continue;
    }
    size_t candidate = tn_selfhost_skip_space(source, length, offset + 6);
    static const char *const declaration_words[] = {"function", "struct", "enum", "class",
                                                     "interface", "type", "const", "static"};
    for (size_t word = 0; word < sizeof(declaration_words) / sizeof(declaration_words[0]); ++word) {
      if (tn_selfhost_word_at(source, length, candidate, declaration_words[word])) {
        candidate = tn_selfhost_skip_space(source, length,
                                            candidate + strlen(declaration_words[word]));
        break;
      }
    }
    if (candidate + name_length <= length && memcmp(source + candidate, name, name_length) == 0 &&
        (candidate + name_length == length ||
         !tn_selfhost_identifier_continue(source[candidate + name_length]))) {
      return 1;
    }
  }
  return 0;
}

static size_t tn_selfhost_skip_quoted(const uint8_t *source, size_t length, size_t offset,
                                      uint8_t quote) {
  offset += 1;
  while (offset < length) {
    if (source[offset] == '\\') {
      offset += offset + 1 < length ? 2 : 1;
      continue;
    }
    if (source[offset] == quote) {
      return offset + 1;
    }
    offset += 1;
  }
  return length;
}

static size_t tn_selfhost_skip_comment(const uint8_t *source, size_t length, size_t offset) {
  if (offset + 1 >= length || source[offset] != '/') {
    return offset;
  }
  if (source[offset + 1] == '/') {
    offset += 2;
    while (offset < length && source[offset] != '\n') {
      offset += 1;
    }
    return offset;
  }
  if (source[offset + 1] != '*') {
    return offset;
  }
  size_t nesting = 1;
  offset += 2;
  while (offset < length && nesting > 0) {
    if (offset + 1 < length && source[offset] == '/' && source[offset + 1] == '*') {
      nesting += 1;
      offset += 2;
    } else if (offset + 1 < length && source[offset] == '*' && source[offset + 1] == '/') {
      nesting -= 1;
      offset += 2;
    } else {
      offset += 1;
    }
  }
  return offset;
}

int32_t tn_selfhost_validate_imports(const char *input_path, const uint8_t *source, size_t length) {
  if (input_path == NULL || (source == NULL && length != 0)) {
    return -EINVAL;
  }
  for (size_t offset = 0; offset < length; ++offset) {
    size_t skipped = tn_selfhost_skip_comment(source, length, offset);
    if (skipped != offset) {
      offset = skipped == 0 ? 0 : skipped - 1;
      continue;
    }
    if (source[offset] == '"' || source[offset] == '\'' || source[offset] == '`') {
      size_t end = tn_selfhost_skip_quoted(source, length, offset, source[offset]);
      offset = end == 0 ? 0 : end - 1;
      continue;
    }
    if (!tn_selfhost_word_at(source, length, offset, "import")) {
      continue;
    }
    size_t statement_end = offset + 6;
    while (statement_end < length && source[statement_end] != ';') {
      statement_end += 1;
    }
    size_t quote = offset + 6;
    while (quote < statement_end && source[quote] != '"' && source[quote] != '\'') {
      quote += 1;
    }
    if (quote >= statement_end) {
      offset = statement_end;
      continue;
    }
    uint8_t terminator = source[quote];
    size_t path_end = quote + 1;
    while (path_end < statement_end && source[path_end] != terminator) {
      path_end += 1;
    }
    if (path_end >= statement_end || path_end == quote + 1 || source[quote + 1] != '.') {
      offset = statement_end;
      continue;
    }
    size_t opening = offset + 6;
    while (opening < statement_end && source[opening] != '{') {
      opening += 1;
    }
    if (opening >= statement_end) {
      offset = statement_end;
      continue;
    }
    size_t closing = opening + 1;
    while (closing < statement_end && source[closing] != '}') {
      closing += 1;
    }
    if (closing >= statement_end) {
      return -EINVAL;
    }
    char path[PATH_MAX];
    const char *slash = strrchr(input_path, '/');
    size_t directory_length = slash == NULL ? 0 : (size_t)(slash - input_path + 1);
    if (directory_length + (path_end - quote - 1) + 4 >= sizeof(path)) {
      return -ENAMETOOLONG;
    }
    memcpy(path, input_path, directory_length);
    size_t path_length = directory_length;
    memcpy(path + path_length, source + quote + 1, path_end - quote - 1);
    path_length += path_end - quote - 1;
    if (path_length < 3 || memcmp(path + path_length - 3, ".tn", 3) != 0) {
      memcpy(path + path_length, ".tn", 3);
      path_length += 3;
    }
    path[path_length] = '\0';
    uint8_t *imported_source = NULL;
    size_t imported_length = 0;
    int status = tn_selfhost_read(path, &imported_source, &imported_length);
    if (status != 0) {
      return status;
    }
    size_t cursor = opening + 1;
    while (cursor < closing) {
      cursor = tn_selfhost_skip_space(source, closing, cursor);
      if (cursor >= closing) {
        break;
      }
      if (!tn_selfhost_identifier_start(source[cursor])) {
        cursor += 1;
        continue;
      }
      size_t name_start = cursor;
      while (cursor < closing && tn_selfhost_identifier_continue(source[cursor])) {
        cursor += 1;
      }
      size_t name_length = cursor - name_start;
      size_t alias = tn_selfhost_skip_space(source, closing, cursor);
      if (alias + 2 <= closing && source[alias] == 'a' && source[alias + 1] == 's') {
        cursor = alias + 2;
        while (cursor < closing && source[cursor] != ',') {
          cursor += 1;
        }
      }
      if (!tn_selfhost_exported_name(imported_source, imported_length, source + name_start,
                                     name_length)) {
        free(imported_source);
        return 1;
      }
      while (cursor < closing && source[cursor] != ',') {
        cursor += 1;
      }
      if (cursor < closing && source[cursor] == ',') {
        cursor += 1;
      }
    }
    free(imported_source);
    offset = statement_end;
  }
  return 0;
}

int32_t tn_selfhost_write_docs(const char *output_path, const uint8_t *source, size_t length) {
  if (output_path == NULL || (source == NULL && length != 0)) {
    return -EINVAL;
  }
  int output = open(output_path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (output < 0) {
    return -errno;
  }
  static const char header[] = "# TypeNative API\n\n";
  int status = tn_io_write_all(output, header, sizeof(header) - 1);
  for (size_t offset = 0; status == 0 && offset < length; ++offset) {
    if (!tn_selfhost_word_at(source, length, offset, "export")) {
      continue;
    }
    size_t candidate = tn_selfhost_skip_space(source, length, offset + 6);
    static const char *const declaration_words[] = {"function", "struct", "enum", "class",
                                                     "interface", "type", "const", "static"};
    for (size_t word = 0; word < sizeof(declaration_words) / sizeof(declaration_words[0]); ++word) {
      if (tn_selfhost_word_at(source, length, candidate, declaration_words[word])) {
        candidate = tn_selfhost_skip_space(source, length,
                                            candidate + strlen(declaration_words[word]));
        break;
      }
    }
    if (candidate >= length || !tn_selfhost_identifier_start(source[candidate])) {
      continue;
    }
    size_t name_end = candidate + 1;
    while (name_end < length && tn_selfhost_identifier_continue(source[name_end])) {
      name_end += 1;
    }
    char line[PATH_MAX];
    int written = snprintf(line, sizeof(line), "## %.*s\n\n", (int)(name_end - candidate),
                           source + candidate);
    if (written < 0 || (size_t)written >= sizeof(line)) {
      status = -EOVERFLOW;
      break;
    }
    status = tn_io_write_all(output, line, (size_t)written);
    offset = name_end;
  }
  if (close(output) != 0 && status == 0) {
    status = -errno;
  }
  return status;
}

static int tn_selfhost_lsp_response(int64_t id, const char *result) {
  char body[1024];
  int body_length = snprintf(body, sizeof(body), "{\"jsonrpc\":\"2.0\",\"id\":%" PRId64
                                             ",\"result\":%s}",
                             id, result);
  if (body_length < 0 || (size_t)body_length >= sizeof(body)) {
    return -EOVERFLOW;
  }
  char header[128];
  int header_length = snprintf(header, sizeof(header), "Content-Length: %d\r\n\r\n", body_length);
  if (header_length < 0 || (size_t)header_length >= sizeof(header)) {
    return -EOVERFLOW;
  }
  if (tn_io_write_all(STDOUT_FILENO, header, (size_t)header_length) != 0) {
    return -EIO;
  }
  return tn_io_write_all(STDOUT_FILENO, body, (size_t)body_length);
}

int32_t tn_selfhost_lsp_run(void) {
  char header[256];
  for (;;) {
    size_t content_length = 0;
    int got_header = 0;
    while (fgets(header, sizeof(header), stdin) != NULL) {
      if (strncmp(header, "Content-Length:", 15) == 0) {
        unsigned long parsed = 0;
        if (sscanf(header + 15, "%lu", &parsed) != 1 || parsed > 16U * 1024U * 1024U) {
          return -EOVERFLOW;
        }
        content_length = (size_t)parsed;
        got_header = 1;
      }
      if (strcmp(header, "\r\n") == 0 || strcmp(header, "\n") == 0) {
        break;
      }
    }
    if (!got_header) {
      return 0;
    }
    char *body = malloc(content_length + 1);
    if (body == NULL) {
      return -ENOMEM;
    }
    size_t received = 0;
    while (received < content_length) {
      size_t count = fread(body + received, 1, content_length - received, stdin);
      if (count == 0) {
        free(body);
        return -EIO;
      }
      received += count;
    }
    body[content_length] = '\0';
    int64_t id = 0;
    const char *id_marker = strstr(body, "\"id\"");
    if (id_marker != NULL) {
      (void)sscanf(id_marker + 4, " : %" SCNd64, &id);
    }
    int status = 0;
    if (strstr(body, "\"method\":\"exit\"") != NULL) {
      free(body);
      return 0;
    }
    if (strstr(body, "\"method\":\"initialize\"") != NULL) {
      status = tn_selfhost_lsp_response(
          id, "{\"capabilities\":{\"textDocumentSync\":1,\"diagnosticProvider\":{}}}");
    } else if (strstr(body, "\"method\":\"shutdown\"") != NULL) {
      status = tn_selfhost_lsp_response(id, "null");
    } else if (id_marker != NULL) {
      status = tn_selfhost_lsp_response(id, "null");
    }
    free(body);
    if (status != 0) {
      return status;
    }
  }
}

int32_t tn_selfhost_byte_n(const uint8_t *source, size_t length, size_t offset) {
  if (source == NULL || offset >= length) {
    return -1;
  }
  return source[offset];
}

uint64_t tn_selfhost_hash_source(const uint8_t *source, size_t length) {
  if (source == NULL && length != 0) {
    return 0;
  }
  uint64_t hash = UINT64_C(1469598103934665603);
  for (size_t offset = 0; offset < length; ++offset) {
    hash ^= source[offset];
    hash *= UINT64_C(1099511628211);
  }
  return hash;
}

uint64_t tn_selfhost_hash_declaration(const uint8_t *source, size_t length, size_t start, size_t end) {
  if (source == NULL || start >= end || end > length) {
    return 0;
  }
  uint64_t hash = tn_selfhost_hash_source(source, length);
  hash ^= tn_selfhost_hash_source(source + start, end - start);
  hash *= UINT64_C(1099511628211);
  hash ^= (uint64_t)start;
  hash *= UINT64_C(1099511628211);
  hash ^= (uint64_t)end;
  return hash;
}

uint64_t tn_selfhost_hash_declaration_with_source_identity(const uint8_t *source, size_t length,
                                                           size_t start, size_t end,
                                                           uint64_t source_identity) {
  if (source == NULL || start >= end || end > length) {
    return 0;
  }
  uint64_t hash = source_identity ^ tn_selfhost_hash_source(source + start, end - start);
  hash *= UINT64_C(1099511628211);
  hash ^= (uint64_t)start;
  hash *= UINT64_C(1099511628211);
  hash ^= (uint64_t)end;
  return hash;
}

int32_t tn_selfhost_word_occurs_after(const uint8_t *source, size_t length, size_t word_start,
                                      size_t word_end, size_t after) {
  if (source == NULL || word_end <= word_start || word_end > length || after > length) {
    return 0;
  }
  size_t word_length = word_end - word_start;
  for (size_t offset = after; offset + word_length <= length; ++offset) {
    if (offset > 0 &&
        (isalnum((unsigned char)source[offset - 1]) || source[offset - 1] == '_')) {
      continue;
    }
    if (offset + word_length < length &&
        (isalnum((unsigned char)source[offset + word_length]) ||
         source[offset + word_length] == '_')) {
      continue;
    }
    if (memcmp(source + offset, source + word_start, word_length) == 0) {
      return 1;
    }
  }
  return 0;
}

static int tn_selfhost_numeric_digit_value(uint8_t byte) {
  if (byte >= '0' && byte <= '9') {
    return (int)(byte - '0');
  }
  if (byte >= 'a' && byte <= 'f') {
    return (int)(byte - 'a') + 10;
  }
  if (byte >= 'A' && byte <= 'F') {
    return (int)(byte - 'A') + 10;
  }
  return -1;
}

static int tn_selfhost_numeric_suffix_is_integer(const uint8_t *source, size_t start,
                                                 size_t end) {
  static const char *const suffixes[] = {"i8",   "i16",  "i32",  "i64",  "i128", "isize",
                                         "u8",   "u16",  "u32",  "u64",  "u128", "usize",
                                         "number"};
  for (size_t index = 0; index < sizeof(suffixes) / sizeof(suffixes[0]); ++index) {
    size_t length = strlen(suffixes[index]);
    if (end - start == length && memcmp(source + start, suffixes[index], length) == 0) {
      return 1;
    }
  }
  return 0;
}

static int tn_selfhost_numeric_suffix_is_float(const uint8_t *source, size_t start,
                                               size_t end) {
  return (end - start == 3 && memcmp(source + start, "f32", 3) == 0) ||
         (end - start == 3 && memcmp(source + start, "f64", 3) == 0);
}

int32_t tn_selfhost_validate_numeric_literal(const uint8_t *source, size_t start, size_t end) {
  if (source == NULL || start >= end) {
    return -EINVAL;
  }
  int has_dot_or_exponent = 0;
  int prefixed_integer = (end - start >= 2 && source[start] == '0' &&
                          (source[start + 1] == 'x' || source[start + 1] == 'X' ||
                           source[start + 1] == 'b' || source[start + 1] == 'B' ||
                           source[start + 1] == 'o' || source[start + 1] == 'O'));
  for (size_t index = start; index < end; ++index) {
    if (source[index] == '.') {
      has_dot_or_exponent = 1;
      break;
    }
    if (!prefixed_integer && (source[index] == 'e' || source[index] == 'E') && index + 1 < end) {
      size_t next = index + 1;
      if (source[next] == '+' || source[next] == '-') {
        next += 1;
      }
      if (next < end && source[next] >= '0' && source[next] <= '9') {
        has_dot_or_exponent = 1;
        break;
      }
    }
  }
  if (has_dot_or_exponent) {
    if ((end - start >= 2 && source[start] == '0') &&
        (source[start + 1] == 'x' || source[start + 1] == 'X' || source[start + 1] == 'b' ||
         source[start + 1] == 'B' || source[start + 1] == 'o' || source[start + 1] == 'O')) {
      return -EINVAL;
    }
    size_t cursor = start;
    size_t mantissa_digits = 0;
    int dot_seen = 0;
    int previous_digit = 0;
    while (cursor < end) {
      uint8_t byte = source[cursor];
      if (byte >= '0' && byte <= '9') {
        mantissa_digits += 1;
        previous_digit = 1;
        cursor += 1;
        continue;
      }
      if (byte == '_') {
        if (!previous_digit || cursor + 1 >= end || source[cursor + 1] < '0' ||
            source[cursor + 1] > '9') {
          return -EINVAL;
        }
        previous_digit = 0;
        cursor += 1;
        continue;
      }
      if (byte == '.' && dot_seen == 0) {
        dot_seen = 1;
        previous_digit = 0;
        cursor += 1;
        continue;
      }
      break;
    }
    if (mantissa_digits == 0 || (dot_seen == 0 && cursor == start)) {
      return -EINVAL;
    }
    int exponent_seen = 0;
    if (cursor < end && (source[cursor] == 'e' || source[cursor] == 'E')) {
      exponent_seen = 1;
      if (!previous_digit) {
        return -EINVAL;
      }
      cursor += 1;
      if (cursor < end && (source[cursor] == '+' || source[cursor] == '-')) {
        cursor += 1;
      }
      size_t exponent_digits = 0;
      previous_digit = 0;
      while (cursor < end) {
        uint8_t byte = source[cursor];
        if (byte >= '0' && byte <= '9') {
          exponent_digits += 1;
          previous_digit = 1;
          cursor += 1;
          continue;
        }
        if (byte == '_') {
          if (!previous_digit || cursor + 1 >= end || source[cursor + 1] < '0' ||
              source[cursor + 1] > '9') {
            return -EINVAL;
          }
          previous_digit = 0;
          cursor += 1;
          continue;
        }
        break;
      }
      if (exponent_digits == 0 || !previous_digit) {
        return -EINVAL;
      }
    }
    if (dot_seen == 0 && exponent_seen == 0) {
      return -EINVAL;
    }
    return (cursor == end || tn_selfhost_numeric_suffix_is_float(source, cursor, end)) ? 0 : -EINVAL;
  }

  unsigned base = 10;
  size_t cursor = start;
  if (end - start >= 2 && source[start] == '0') {
    if (source[start + 1] == 'x' || source[start + 1] == 'X') {
      base = 16;
      cursor += 2;
    } else if (source[start + 1] == 'b' || source[start + 1] == 'B') {
      base = 2;
      cursor += 2;
    } else if (source[start + 1] == 'o' || source[start + 1] == 'O') {
      base = 8;
      cursor += 2;
    }
  }
  size_t digits = 0;
  int previous_digit = 0;
  while (cursor < end) {
    uint8_t byte = source[cursor];
    int digit = tn_selfhost_numeric_digit_value(byte);
    if (digit >= 0 && (unsigned)digit < base) {
      digits += 1;
      previous_digit = 1;
      cursor += 1;
      continue;
    }
    if (byte == '_') {
      if (!previous_digit || cursor + 1 >= end) {
        return -EINVAL;
      }
      int next_digit = tn_selfhost_numeric_digit_value(source[cursor + 1]);
      if (next_digit < 0 || (unsigned)next_digit >= base) {
        return -EINVAL;
      }
      previous_digit = 0;
      cursor += 1;
      continue;
    }
    break;
  }
  if (digits == 0 || !previous_digit ||
      (cursor != end && !tn_selfhost_numeric_suffix_is_integer(source, cursor, end))) {
    return -EINVAL;
  }
  return 0;
}

int32_t tn_selfhost_parse_i32(const uint8_t *source, size_t start, size_t end, int32_t *value) {
  if (source == NULL || value == NULL || end <= start) {
    return -EINVAL;
  }
  int negative = 0;
  size_t offset = start;
  if (source[offset] == '-') {
    negative = 1;
    offset += 1;
  }
  if (offset >= end || source[offset] < '0' || source[offset] > '9') {
    return -EINVAL;
  }
  unsigned base = 10;
  if (offset + 1 < end && source[offset] == '0') {
    if (source[offset + 1] == 'x' || source[offset + 1] == 'X') {
      base = 16;
      offset += 2;
    } else if (source[offset + 1] == 'b' || source[offset + 1] == 'B') {
      base = 2;
      offset += 2;
    } else if (source[offset + 1] == 'o' || source[offset + 1] == 'O') {
      base = 8;
      offset += 2;
    }
  }
  uint64_t magnitude = 0;
  int digit_count = 0;
  int separator_pending = 0;
  while (offset < end) {
    uint8_t byte = source[offset];
    if (byte == '_') {
      if (digit_count == 0 || separator_pending) {
        return -EINVAL;
      }
      separator_pending = 1;
      offset += 1;
      continue;
    }
    unsigned digit = UINT_MAX;
    if (byte >= '0' && byte <= '9') {
      digit = (unsigned)(byte - '0');
    } else if (byte >= 'a' && byte <= 'f') {
      digit = (unsigned)(byte - 'a') + 10U;
    } else if (byte >= 'A' && byte <= 'F') {
      digit = (unsigned)(byte - 'A') + 10U;
    }
    if (digit >= base) {
      break;
    }
    separator_pending = 0;
    if (magnitude > (UINT64_C(2147483648) - (uint64_t)digit) / (uint64_t)base) {
      return -ERANGE;
    }
    magnitude = magnitude * (uint64_t)base + (uint64_t)digit;
    offset += 1;
    digit_count += 1;
  }
  if (separator_pending) {
    return -EINVAL;
  }
  if (offset < end) {
    size_t suffix_start = offset;
    while (offset < end && ((source[offset] >= 'a' && source[offset] <= 'z') ||
                            (source[offset] >= 'A' && source[offset] <= 'Z') ||
                            (source[offset] >= '0' && source[offset] <= '9') || source[offset] == '_')) {
      offset += 1;
    }
    size_t suffix_length = offset - suffix_start;
    static const char *const suffixes[] = {
        "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize", "number",
    };
    int recognized = 0;
    for (size_t index = 0; index < sizeof(suffixes) / sizeof(suffixes[0]); ++index) {
      if (suffix_length == strlen(suffixes[index]) &&
          memcmp(source + suffix_start, suffixes[index], suffix_length) == 0) {
        recognized = 1;
        break;
      }
    }
    if (suffix_length == 0 || offset != end || recognized == 0) {
      return -EINVAL;
    }
  }
  if (digit_count == 0) {
    return -EINVAL;
  }
  if (negative) {
    if (magnitude == UINT64_C(2147483648)) {
      *value = INT32_MIN;
    } else {
      *value = -(int32_t)magnitude;
    }
  } else {
    if (magnitude > UINT64_C(2147483647)) {
      return -ERANGE;
    }
    *value = (int32_t)magnitude;
  }
  return 0;
}

int32_t tn_selfhost_llvm_major(void) {
  typedef void (*llvm_get_version)(unsigned *, unsigned *, unsigned *, unsigned *);
  static const char *const candidates[] = {
      "/opt/homebrew/opt/llvm/lib/libLLVM.dylib", "libLLVM.dylib", "libLLVM.so.22",
      "libLLVM-22.so", "libLLVM.so"};
  for (size_t index = 0; index < sizeof(candidates) / sizeof(candidates[0]); ++index) {
    void *library = dlopen(candidates[index], RTLD_LAZY | RTLD_LOCAL);
    if (library == NULL) {
      continue;
    }
    union {
      void *object;
      llvm_get_version function;
    } symbol = {.object = dlsym(library, "LLVMGetVersion")};
    if (symbol.function != NULL) {
      unsigned major = 0;
      unsigned minor = 0;
      unsigned patch = 0;
      unsigned build = 0;
      symbol.function(&major, &minor, &patch, &build);
      dlclose(library);
      return (int32_t)major;
    }
    dlclose(library);
  }
  return -1;
}

typedef void *(*tn_llvm_context_create_fn)(void);
typedef void *(*tn_llvm_module_create_fn)(const char *, void *);
typedef void (*tn_llvm_module_dispose_fn)(void *);
typedef void (*tn_llvm_context_dispose_fn)(void *);
typedef void *(*tn_llvm_int32_type_fn)(void *);
typedef void *(*tn_llvm_void_type_fn)(void *);
typedef void *(*tn_llvm_function_type_fn)(void *, void **, unsigned, int);
typedef void *(*tn_llvm_add_function_fn)(void *, const char *, void *);
typedef void *(*tn_llvm_get_param_fn)(void *, unsigned);
typedef void *(*tn_llvm_append_basic_block_fn)(void *, void *, const char *);
typedef void *(*tn_llvm_create_builder_fn)(void *);
typedef void (*tn_llvm_position_builder_fn)(void *, void *);
typedef void *(*tn_llvm_const_int_fn)(void *, unsigned long long, int);
typedef void *(*tn_llvm_build_binary_fn)(void *, void *, void *, const char *);
typedef void *(*tn_llvm_build_unary_fn)(void *, void *, const char *);
typedef void *(*tn_llvm_build_icmp_fn)(void *, int, void *, void *, const char *);
typedef void *(*tn_llvm_build_select_fn)(void *, void *, void *, void *, const char *);
typedef void *(*tn_llvm_build_cast_fn)(void *, void *, void *, const char *);
typedef void *(*tn_llvm_build_call2_fn)(void *, void *, void *, void **, unsigned, const char *);
typedef void *(*tn_llvm_build_ret_fn)(void *, void *);
typedef void *(*tn_llvm_build_ret_void_fn)(void *);
typedef void (*tn_llvm_dispose_builder_fn)(void *);
typedef int (*tn_llvm_verify_module_fn)(void *, unsigned, char **);
typedef char *(*tn_llvm_print_module_fn)(void *);
typedef void (*tn_llvm_dispose_message_fn)(char *);
typedef int (*tn_llvm_write_bitcode_fn)(void *, const char *);
typedef char *(*tn_llvm_get_default_target_triple_fn)(void);
typedef int (*tn_llvm_get_target_from_triple_fn)(const char *, void **, char **);
typedef void *(*tn_llvm_create_target_machine_options_fn)(void);
typedef void (*tn_llvm_dispose_target_machine_options_fn)(void *);
typedef void *(*tn_llvm_create_target_machine_with_options_fn)(void *, const char *, void *);
typedef void (*tn_llvm_dispose_target_machine_fn)(void *);
typedef int (*tn_llvm_target_machine_emit_to_file_fn)(void *, void *, const char *, int, char **);
typedef void (*tn_llvm_initialize_target_fn)(void);

typedef struct {
  void *library;
  tn_llvm_context_create_fn context_create;
  tn_llvm_module_create_fn module_create;
  tn_llvm_module_dispose_fn module_dispose;
  tn_llvm_context_dispose_fn context_dispose;
  tn_llvm_int32_type_fn int32_type;
  tn_llvm_void_type_fn void_type;
  tn_llvm_function_type_fn function_type;
  tn_llvm_add_function_fn add_function;
  tn_llvm_get_param_fn get_param;
  tn_llvm_append_basic_block_fn append_basic_block;
  tn_llvm_create_builder_fn create_builder;
  tn_llvm_position_builder_fn position_builder;
  tn_llvm_const_int_fn const_int;
  tn_llvm_build_binary_fn build_add;
  tn_llvm_build_binary_fn build_sub;
  tn_llvm_build_binary_fn build_mul;
  tn_llvm_build_binary_fn build_sdiv;
  tn_llvm_build_binary_fn build_srem;
  tn_llvm_build_unary_fn build_neg;
  tn_llvm_build_icmp_fn build_icmp;
  tn_llvm_build_select_fn build_select;
  tn_llvm_build_cast_fn build_zext;
  tn_llvm_build_call2_fn build_call2;
  tn_llvm_build_ret_fn build_ret;
  tn_llvm_build_ret_void_fn build_ret_void;
  tn_llvm_dispose_builder_fn dispose_builder;
  tn_llvm_verify_module_fn verify_module;
  tn_llvm_print_module_fn print_module;
  tn_llvm_dispose_message_fn dispose_message;
  tn_llvm_write_bitcode_fn write_bitcode;
  tn_llvm_get_default_target_triple_fn get_default_target_triple;
  tn_llvm_get_target_from_triple_fn get_target_from_triple;
  tn_llvm_create_target_machine_options_fn create_target_machine_options;
  tn_llvm_dispose_target_machine_options_fn dispose_target_machine_options;
  tn_llvm_create_target_machine_with_options_fn create_target_machine_with_options;
  tn_llvm_dispose_target_machine_fn dispose_target_machine;
  tn_llvm_target_machine_emit_to_file_fn target_machine_emit_to_file;
} tn_selfhost_llvm_api;

static tn_selfhost_llvm_api tn_selfhost_llvm_api_state;
static pthread_once_t tn_selfhost_llvm_api_once = PTHREAD_ONCE_INIT;

static void tn_selfhost_llvm_load_api_once(void) {
  static const char *const candidates[] = {
      "/opt/homebrew/opt/llvm/lib/libLLVM.dylib", "libLLVM.dylib", "libLLVM.so.22",
      "libLLVM-22.so", "libLLVM.so"};
  for (size_t index = 0; index < sizeof(candidates) / sizeof(candidates[0]); ++index) {
    void *library = dlopen(candidates[index], RTLD_LAZY | RTLD_LOCAL);
    if (library == NULL) {
      continue;
    }
    union {
      void *object;
      tn_llvm_context_create_fn function;
    } context_create = {.object = dlsym(library, "LLVMContextCreate")};
    union {
      void *object;
      tn_llvm_module_create_fn function;
    } module_create = {.object = dlsym(library, "LLVMModuleCreateWithNameInContext")};
    union {
      void *object;
      tn_llvm_module_dispose_fn function;
    } module_dispose = {.object = dlsym(library, "LLVMDisposeModule")};
    union {
      void *object;
      tn_llvm_context_dispose_fn function;
    } context_dispose = {.object = dlsym(library, "LLVMContextDispose")};
    union {
      void *object;
      tn_llvm_int32_type_fn function;
    } int32_type = {.object = dlsym(library, "LLVMInt32TypeInContext")};
    union {
      void *object;
      tn_llvm_void_type_fn function;
    } void_type = {.object = dlsym(library, "LLVMVoidTypeInContext")};
    union {
      void *object;
      tn_llvm_function_type_fn function;
    } function_type = {.object = dlsym(library, "LLVMFunctionType")};
    union {
      void *object;
      tn_llvm_add_function_fn function;
    } add_function = {.object = dlsym(library, "LLVMAddFunction")};
    union {
      void *object;
      tn_llvm_get_param_fn function;
    } get_param = {.object = dlsym(library, "LLVMGetParam")};
    union {
      void *object;
      tn_llvm_append_basic_block_fn function;
    } append_basic_block = {.object = dlsym(library, "LLVMAppendBasicBlockInContext")};
    union {
      void *object;
      tn_llvm_create_builder_fn function;
    } create_builder = {.object = dlsym(library, "LLVMCreateBuilderInContext")};
    union {
      void *object;
      tn_llvm_position_builder_fn function;
    } position_builder = {.object = dlsym(library, "LLVMPositionBuilderAtEnd")};
    union {
      void *object;
      tn_llvm_const_int_fn function;
    } const_int = {.object = dlsym(library, "LLVMConstInt")};
    union {
      void *object;
      tn_llvm_build_binary_fn function;
    } build_add = {.object = dlsym(library, "LLVMBuildAdd")};
    union {
      void *object;
      tn_llvm_build_binary_fn function;
    } build_sub = {.object = dlsym(library, "LLVMBuildSub")};
    union {
      void *object;
      tn_llvm_build_binary_fn function;
    } build_mul = {.object = dlsym(library, "LLVMBuildMul")};
    union {
      void *object;
      tn_llvm_build_binary_fn function;
    } build_sdiv = {.object = dlsym(library, "LLVMBuildSDiv")};
    union {
      void *object;
      tn_llvm_build_binary_fn function;
    } build_srem = {.object = dlsym(library, "LLVMBuildSRem")};
    union {
      void *object;
      tn_llvm_build_unary_fn function;
    } build_neg = {.object = dlsym(library, "LLVMBuildNeg")};
    union {
      void *object;
      tn_llvm_build_icmp_fn function;
    } build_icmp = {.object = dlsym(library, "LLVMBuildICmp")};
    union {
      void *object;
      tn_llvm_build_select_fn function;
    } build_select = {.object = dlsym(library, "LLVMBuildSelect")};
    union {
      void *object;
      tn_llvm_build_cast_fn function;
    } build_zext = {.object = dlsym(library, "LLVMBuildZExt")};
    union {
      void *object;
      tn_llvm_build_call2_fn function;
    } build_call2 = {.object = dlsym(library, "LLVMBuildCall2")};
    union {
      void *object;
      tn_llvm_build_ret_fn function;
    } build_ret = {.object = dlsym(library, "LLVMBuildRet")};
    union {
      void *object;
      tn_llvm_build_ret_void_fn function;
    } build_ret_void = {.object = dlsym(library, "LLVMBuildRetVoid")};
    union {
      void *object;
      tn_llvm_dispose_builder_fn function;
    } dispose_builder = {.object = dlsym(library, "LLVMDisposeBuilder")};
    union {
      void *object;
      tn_llvm_verify_module_fn function;
    } verify_module = {.object = dlsym(library, "LLVMVerifyModule")};
    union {
      void *object;
      tn_llvm_print_module_fn function;
    } print_module = {.object = dlsym(library, "LLVMPrintModuleToString")};
    union {
      void *object;
      tn_llvm_dispose_message_fn function;
    } dispose_message = {.object = dlsym(library, "LLVMDisposeMessage")};
    union {
      void *object;
      tn_llvm_write_bitcode_fn function;
    } write_bitcode = {.object = dlsym(library, "LLVMWriteBitcodeToFile")};
    union {
      void *object;
      tn_llvm_get_default_target_triple_fn function;
    } get_default_target_triple = {.object = dlsym(library, "LLVMGetDefaultTargetTriple")};
    union {
      void *object;
      tn_llvm_get_target_from_triple_fn function;
    } get_target_from_triple = {.object = dlsym(library, "LLVMGetTargetFromTriple")};
    union {
      void *object;
      tn_llvm_create_target_machine_options_fn function;
    } create_target_machine_options = {.object = dlsym(library, "LLVMCreateTargetMachineOptions")};
    union {
      void *object;
      tn_llvm_dispose_target_machine_options_fn function;
    } dispose_target_machine_options = {.object = dlsym(library, "LLVMDisposeTargetMachineOptions")};
    union {
      void *object;
      tn_llvm_create_target_machine_with_options_fn function;
    } create_target_machine_with_options = {.object = dlsym(library, "LLVMCreateTargetMachineWithOptions")};
    union {
      void *object;
      tn_llvm_dispose_target_machine_fn function;
    } dispose_target_machine = {.object = dlsym(library, "LLVMDisposeTargetMachine")};
    union {
      void *object;
      tn_llvm_target_machine_emit_to_file_fn function;
    } target_machine_emit_to_file = {.object = dlsym(library, "LLVMTargetMachineEmitToFile")};
#if defined(__aarch64__) || defined(__arm64__)
    union {
      void *object;
      tn_llvm_initialize_target_fn function;
    } initialize_target_info = {.object = dlsym(library, "LLVMInitializeAArch64TargetInfo")};
    union {
      void *object;
      tn_llvm_initialize_target_fn function;
    } initialize_target = {.object = dlsym(library, "LLVMInitializeAArch64Target")};
    union {
      void *object;
      tn_llvm_initialize_target_fn function;
    } initialize_target_mc = {.object = dlsym(library, "LLVMInitializeAArch64TargetMC")};
    union {
      void *object;
      tn_llvm_initialize_target_fn function;
    } initialize_asm_printer = {.object = dlsym(library, "LLVMInitializeAArch64AsmPrinter")};
#elif defined(__x86_64__)
    union {
      void *object;
      tn_llvm_initialize_target_fn function;
    } initialize_target_info = {.object = dlsym(library, "LLVMInitializeX86TargetInfo")};
    union {
      void *object;
      tn_llvm_initialize_target_fn function;
    } initialize_target = {.object = dlsym(library, "LLVMInitializeX86Target")};
    union {
      void *object;
      tn_llvm_initialize_target_fn function;
    } initialize_target_mc = {.object = dlsym(library, "LLVMInitializeX86TargetMC")};
    union {
      void *object;
      tn_llvm_initialize_target_fn function;
    } initialize_asm_printer = {.object = dlsym(library, "LLVMInitializeX86AsmPrinter")};
#else
    continue;
#endif
    if (context_create.function == NULL || module_create.function == NULL ||
        module_dispose.function == NULL || context_dispose.function == NULL ||
        int32_type.function == NULL || void_type.function == NULL || function_type.function == NULL ||
        add_function.function == NULL || get_param.function == NULL || append_basic_block.function == NULL ||
        create_builder.function == NULL || position_builder.function == NULL ||
        const_int.function == NULL || build_add.function == NULL || build_sub.function == NULL ||
        build_mul.function == NULL || build_sdiv.function == NULL || build_srem.function == NULL ||
        build_neg.function == NULL || build_icmp.function == NULL || build_select.function == NULL ||
        build_zext.function == NULL ||
        build_call2.function == NULL || build_ret.function == NULL ||
        build_ret_void.function == NULL ||
        dispose_builder.function == NULL || verify_module.function == NULL ||
        print_module.function == NULL || dispose_message.function == NULL || write_bitcode.function == NULL ||
        get_default_target_triple.function == NULL || get_target_from_triple.function == NULL ||
        create_target_machine_options.function == NULL || dispose_target_machine_options.function == NULL ||
        create_target_machine_with_options.function == NULL ||
        dispose_target_machine.function == NULL ||
        target_machine_emit_to_file.function == NULL || initialize_target_info.function == NULL ||
        initialize_target.function == NULL || initialize_target_mc.function == NULL ||
        initialize_asm_printer.function == NULL) {
      dlclose(library);
      continue;
    }
    initialize_target_info.function();
    initialize_target.function();
    initialize_target_mc.function();
    initialize_asm_printer.function();
    tn_selfhost_llvm_api_state.library = library;
    tn_selfhost_llvm_api_state.context_create = context_create.function;
    tn_selfhost_llvm_api_state.module_create = module_create.function;
    tn_selfhost_llvm_api_state.module_dispose = module_dispose.function;
    tn_selfhost_llvm_api_state.context_dispose = context_dispose.function;
    tn_selfhost_llvm_api_state.int32_type = int32_type.function;
    tn_selfhost_llvm_api_state.void_type = void_type.function;
    tn_selfhost_llvm_api_state.function_type = function_type.function;
    tn_selfhost_llvm_api_state.add_function = add_function.function;
    tn_selfhost_llvm_api_state.get_param = get_param.function;
    tn_selfhost_llvm_api_state.append_basic_block = append_basic_block.function;
    tn_selfhost_llvm_api_state.create_builder = create_builder.function;
    tn_selfhost_llvm_api_state.position_builder = position_builder.function;
    tn_selfhost_llvm_api_state.const_int = const_int.function;
    tn_selfhost_llvm_api_state.build_add = build_add.function;
    tn_selfhost_llvm_api_state.build_sub = build_sub.function;
    tn_selfhost_llvm_api_state.build_mul = build_mul.function;
    tn_selfhost_llvm_api_state.build_sdiv = build_sdiv.function;
    tn_selfhost_llvm_api_state.build_srem = build_srem.function;
    tn_selfhost_llvm_api_state.build_neg = build_neg.function;
    tn_selfhost_llvm_api_state.build_icmp = build_icmp.function;
    tn_selfhost_llvm_api_state.build_select = build_select.function;
    tn_selfhost_llvm_api_state.build_zext = build_zext.function;
    tn_selfhost_llvm_api_state.build_call2 = build_call2.function;
    tn_selfhost_llvm_api_state.build_ret = build_ret.function;
    tn_selfhost_llvm_api_state.build_ret_void = build_ret_void.function;
    tn_selfhost_llvm_api_state.dispose_builder = dispose_builder.function;
    tn_selfhost_llvm_api_state.verify_module = verify_module.function;
    tn_selfhost_llvm_api_state.print_module = print_module.function;
    tn_selfhost_llvm_api_state.dispose_message = dispose_message.function;
    tn_selfhost_llvm_api_state.write_bitcode = write_bitcode.function;
    tn_selfhost_llvm_api_state.get_default_target_triple = get_default_target_triple.function;
    tn_selfhost_llvm_api_state.get_target_from_triple = get_target_from_triple.function;
    tn_selfhost_llvm_api_state.create_target_machine_options = create_target_machine_options.function;
    tn_selfhost_llvm_api_state.dispose_target_machine_options = dispose_target_machine_options.function;
    tn_selfhost_llvm_api_state.create_target_machine_with_options = create_target_machine_with_options.function;
    tn_selfhost_llvm_api_state.dispose_target_machine = dispose_target_machine.function;
    tn_selfhost_llvm_api_state.target_machine_emit_to_file = target_machine_emit_to_file.function;
    return;
  }
}

void *tn_selfhost_llvm_context_create(void) {
  pthread_once(&tn_selfhost_llvm_api_once, tn_selfhost_llvm_load_api_once);
  return tn_selfhost_llvm_api_state.context_create == NULL
             ? NULL
             : tn_selfhost_llvm_api_state.context_create();
}

void *tn_selfhost_llvm_module_create(const char *name, void *context) {
  pthread_once(&tn_selfhost_llvm_api_once, tn_selfhost_llvm_load_api_once);
  if (name == NULL || context == NULL || tn_selfhost_llvm_api_state.module_create == NULL) {
    return NULL;
  }
  return tn_selfhost_llvm_api_state.module_create(name, context);
}

void tn_selfhost_llvm_module_dispose(void *module) {
  if (module != NULL && tn_selfhost_llvm_api_state.module_dispose != NULL) {
    tn_selfhost_llvm_api_state.module_dispose(module);
  }
}

void tn_selfhost_llvm_context_dispose(void *context) {
  if (context != NULL && tn_selfhost_llvm_api_state.context_dispose != NULL) {
    tn_selfhost_llvm_api_state.context_dispose(context);
  }
}

int32_t tn_selfhost_llvm_module_roundtrip(const char *name) {
  if (name == NULL) {
    return -EINVAL;
  }
  void *context = tn_selfhost_llvm_context_create();
  if (context == NULL) {
    return -ENOENT;
  }
  void *module = tn_selfhost_llvm_module_create(name, context);
  if (module == NULL) {
    tn_selfhost_llvm_context_dispose(context);
    return -ENOMEM;
  }
  tn_selfhost_llvm_module_dispose(module);
  tn_selfhost_llvm_context_dispose(context);
  return 0;
}

static const char *tn_selfhost_runtime_root(void) {
  const char *configured = getenv("TYPENATIVE_RUNTIME_ROOT");
  if (configured != NULL && configured[0] != '\0') {
    return configured;
  }
  static char derived[PATH_MAX];
  const char *source = __FILE__;
  const char *slash = strrchr(source, '/');
  if (slash == NULL) {
    return "runtime";
  }
  size_t length = (size_t)(slash - source);
  if (length == 0 || length >= sizeof(derived)) {
    return NULL;
  }
  memcpy(derived, source, length);
  derived[length] = '\0';
  return derived;
}

static int32_t tn_selfhost_runtime_path(const char *root, const char *name, char *output, size_t capacity) {
  if (root == NULL || name == NULL || output == NULL || capacity == 0) {
    return -EINVAL;
  }
  int written = snprintf(output, capacity, "%s/%s", root, name);
  if (written < 0) {
    return -EIO;
  }
  return (size_t)written < capacity ? 0 : -ENAMETOOLONG;
}

static int32_t tn_selfhost_wait_command(const char *program, char *const arguments[]) {
  if (program == NULL || arguments == NULL) {
    return -EINVAL;
  }
  pid_t child = 0;
  int spawn_status = posix_spawnp(&child, program, NULL, NULL, arguments, environ);
  if (spawn_status != 0) {
    return -spawn_status;
  }
  int wait_status = 0;
  if (waitpid(child, &wait_status, 0) < 0) {
    return -errno;
  }
  if (WIFEXITED(wait_status) && WEXITSTATUS(wait_status) == 0) {
    return 0;
  }
  return -EIO;
}

static int32_t tn_selfhost_node_include(char *output, size_t capacity) {
  if (output == NULL || capacity == 0) {
    return -EINVAL;
  }
  const char *configured = getenv("NODE_INCLUDE_DIR");
  const char *candidates[] = {
      configured,
      "/opt/homebrew/opt/node@24/include/node",
      "/opt/homebrew/opt/node/include/node",
      "/usr/local/include/node",
      "/usr/include/node",
  };
  for (size_t index = 0; index < sizeof(candidates) / sizeof(candidates[0]); ++index) {
    const char *candidate = candidates[index];
    if (candidate == NULL || candidate[0] == '\0') {
      continue;
    }
    char header[PATH_MAX];
    if (tn_selfhost_runtime_path(candidate, "node_api.h", header, sizeof(header)) != 0 ||
        access(header, R_OK) != 0) {
      continue;
    }
    int written = snprintf(output, capacity, "%s", candidate);
    if (written >= 0 && (size_t)written < capacity) {
      return 0;
    }
    return -ENAMETOOLONG;
  }
  return -ENOENT;
}

static int32_t tn_selfhost_node_declarations(const char *output_path, int32_t returns_void) {
  if (output_path == NULL) {
    return -EINVAL;
  }
  size_t length = strlen(output_path);
  const char *slash = strrchr(output_path, '/');
  const char *dot = strrchr(output_path, '.');
  size_t base_length = length;
  if (dot != NULL && (slash == NULL || dot > slash)) {
    base_length = (size_t)(dot - output_path);
  }
  const char suffix[] = ".d.ts";
  if (base_length > SIZE_MAX - sizeof(suffix)) {
    return -EOVERFLOW;
  }
  char *declarations_path = malloc(base_length + sizeof(suffix));
  if (declarations_path == NULL) {
    return -ENOMEM;
  }
  memcpy(declarations_path, output_path, base_length);
  memcpy(declarations_path + base_length, suffix, sizeof(suffix));
  const char *result_type = returns_void == 0 ? "number" : "void";
  char declarations[128];
  int written = snprintf(declarations, sizeof(declarations), "export function main(): %s;\n", result_type);
  int32_t status = written < 0 || (size_t)written >= sizeof(declarations)
                       ? -EOVERFLOW
                       : tn_selfhost_write(declarations_path, (const uint8_t *)declarations, (size_t)written);
  free(declarations_path);
  return status;
}

static int32_t tn_selfhost_link_node_native(const char *intermediate, const char *output_path,
                                            int32_t returns_void) {
  const char *root = tn_selfhost_runtime_root();
  if (root == NULL) {
    return -ENAMETOOLONG;
  }
  char runtime[PATH_MAX];
  char redis[PATH_MAX];
  if (tn_selfhost_runtime_path(root, "runtime.c", runtime, sizeof(runtime)) != 0 ||
      tn_selfhost_runtime_path(root, "redis.c", redis, sizeof(redis)) != 0) {
    return -ENAMETOOLONG;
  }
  char include[PATH_MAX];
  int32_t include_status = tn_selfhost_node_include(include, sizeof(include));
  if (include_status != 0) {
    return include_status;
  }
  const char *compiler = getenv("TN_CLANG");
  if (compiler == NULL || compiler[0] == '\0') {
    compiler = "clang";
  }
  size_t output_length = strlen(output_path);
  if (output_length > SIZE_MAX - 32) {
    return -EOVERFLOW;
  }
  char *source_template = malloc(output_length + 32);
  char *object_template = malloc(output_length + 32);
  if (source_template == NULL || object_template == NULL) {
    free(source_template);
    free(object_template);
    return -ENOMEM;
  }
  int source_written = snprintf(source_template, output_length + 32, "%s.tn-node-source-XXXXXX", output_path);
  int object_written = snprintf(object_template, output_length + 32, "%s.tn-node-object-XXXXXX", output_path);
  if (source_written < 0 || object_written < 0 || (size_t)source_written >= output_length + 32 ||
      (size_t)object_written >= output_length + 32) {
    free(source_template);
    free(object_template);
    return -EOVERFLOW;
  }
  int source_descriptor = mkstemp(source_template);
  int object_descriptor = mkstemp(object_template);
  if (source_descriptor < 0 || object_descriptor < 0) {
    if (source_descriptor >= 0) {
      close(source_descriptor);
    }
    if (object_descriptor >= 0) {
      close(object_descriptor);
    }
    unlink(source_template);
    unlink(object_template);
    free(source_template);
    free(object_template);
    return -errno;
  }
  close(source_descriptor);
  close(object_descriptor);
  const char *body = returns_void == 0
                         ? "static napi_value tn_main(napi_env env, napi_callback_info info) { (void)info; "
                           "int32_t value = main(); napi_value result; "
                           "if (napi_create_int32(env, value, &result) != napi_ok) return NULL; return result; }\n"
                         : "static napi_value tn_main(napi_env env, napi_callback_info info) { (void)info; "
                           "napi_value result; if (napi_get_undefined(env, &result) != napi_ok) return NULL; "
                           "main(); return result; }\n";
  char wrapper[2048];
  int wrapper_written = snprintf(wrapper, sizeof(wrapper),
                                 "#include <node_api.h>\n#include <stdint.h>\nextern %s main(void);\n%s"
                                 "NAPI_MODULE_INIT() { napi_property_descriptor descriptor = {\"main\", NULL, "
                                 "tn_main, NULL, NULL, NULL, napi_default, NULL}; "
                                 "if (napi_define_properties(env, exports, 1, &descriptor) != napi_ok) return NULL; "
                                 "return exports; }\n",
                                 returns_void == 0 ? "int32_t" : "void", body);
  int32_t status = wrapper_written < 0 || (size_t)wrapper_written >= sizeof(wrapper)
                       ? -EOVERFLOW
                       : tn_selfhost_write(source_template, (const uint8_t *)wrapper, (size_t)wrapper_written);
  if (status == 0) {
    char *compile_arguments[] = {
        (char *)compiler, (char *)"-fPIC", (char *)"-I", include, (char *)"-x", (char *)"c", (char *)"-c",
        source_template,
        (char *)"-o", object_template, NULL,
    };
    status = tn_selfhost_wait_command(compiler, compile_arguments);
  }
  if (status == 0) {
    char *link_arguments[20];
    size_t count = 0;
    link_arguments[count++] = (char *)compiler;
    link_arguments[count++] = (char *)"-fPIC";
    link_arguments[count++] = (char *)"-x";
    link_arguments[count++] = (char *)"assembler";
    link_arguments[count++] = (char *)intermediate;
    link_arguments[count++] = (char *)"-x";
    link_arguments[count++] = (char *)"none";
    link_arguments[count++] = object_template;
    link_arguments[count++] = runtime;
    link_arguments[count++] = redis;
    link_arguments[count++] = (char *)"-pthread";
#if defined(__linux__)
    link_arguments[count++] = (char *)"-ldl";
#endif
#if defined(__APPLE__)
    link_arguments[count++] = (char *)"-bundle";
    link_arguments[count++] = (char *)"-undefined";
    link_arguments[count++] = (char *)"dynamic_lookup";
#else
    link_arguments[count++] = (char *)"-shared";
#endif
    link_arguments[count++] = (char *)"-o";
    link_arguments[count++] = (char *)output_path;
    link_arguments[count] = NULL;
    status = tn_selfhost_wait_command(compiler, link_arguments);
  }
  if (status == 0) {
    status = tn_selfhost_node_declarations(output_path, returns_void);
  }
  unlink(source_template);
  unlink(object_template);
  free(source_template);
  free(object_template);
  return status;
}

static int32_t tn_selfhost_link_native(const char *intermediate, const char *output_path, int32_t product,
                                       int32_t returns_void) {
  const char *root = tn_selfhost_runtime_root();
  if (root == NULL) {
    return -ENAMETOOLONG;
  }
  char startup[PATH_MAX];
  char runtime[PATH_MAX];
  char redis[PATH_MAX];
  if (tn_selfhost_runtime_path(root, "startup.c", startup, sizeof(startup)) != 0 ||
      tn_selfhost_runtime_path(root, "runtime.c", runtime, sizeof(runtime)) != 0 ||
      tn_selfhost_runtime_path(root, "redis.c", redis, sizeof(redis)) != 0) {
    return -ENAMETOOLONG;
  }
  const char *compiler = getenv("TN_CLANG");
  if (compiler == NULL || compiler[0] == '\0') {
    compiler = "clang";
  }
  const char *entry_mode = returns_void == 0 ? "-DTN_ENTRY_I32" : "";
  char *arguments[20];
  size_t count = 0;
  arguments[count++] = (char *)compiler;
  if (product == 5) {
    arguments[count++] = (char *)"-fPIC";
    arguments[count++] = (char *)"-x";
    arguments[count++] = (char *)"assembler";
  }
  arguments[count++] = (char *)intermediate;
  if (product == 4) {
    arguments[count++] = (char *)startup;
    arguments[count++] = (char *)"-DTN_ENTRY=tn_selfhost_entry";
    if (entry_mode[0] != '\0') {
      arguments[count++] = (char *)entry_mode;
    }
  }
  if (product == 5) {
    arguments[count++] = (char *)"-x";
    arguments[count++] = (char *)"c";
  }
  arguments[count++] = (char *)runtime;
  arguments[count++] = (char *)redis;
  arguments[count++] = (char *)"-pthread";
#if defined(__linux__)
  arguments[count++] = (char *)"-ldl";
#endif
  if (product == 5) {
#if defined(__APPLE__)
    arguments[count++] = (char *)"-dynamiclib";
#else
    arguments[count++] = (char *)"-shared";
#endif
  }
  arguments[count++] = (char *)"-o";
  arguments[count++] = (char *)output_path;
  arguments[count] = NULL;
  return tn_selfhost_wait_command(compiler, arguments);
}

static int32_t tn_selfhost_llvm_write_product(tn_selfhost_llvm_api *api, void *module, const char *output_path,
                                               int32_t product, int32_t returns_void) {
  if (product == 0) {
    char *ir = api->print_module(module);
    if (ir == NULL) {
      return -ENOMEM;
    }
    int32_t status = tn_selfhost_write(output_path, (const uint8_t *)ir, strlen(ir));
    api->dispose_message(ir);
    return status;
  }
  if (product == 1) {
    return api->write_bitcode(module, output_path) == 0 ? 0 : -EIO;
  }
  if (product != 2 && product != 3 && product != 4 && product != 5 && product != 6) {
    return -EINVAL;
  }

  /* The target-machine API writes the intermediate product first.  Executable
   * and shared-library linking is deliberately kept here, beside the LLVM
   * binding, so the hosted compiler and the Rust driver use the same runtime
   * startup and support objects. */
  if (product == 4 || product == 5 || product == 6) {
    size_t output_length = strlen(output_path);
    const char *suffix = product == 4 ? ".tn-executable-XXXXXX"
                                      : product == 5 ? ".tn-shared-XXXXXX" : ".tn-node-XXXXXX";
    size_t suffix_length = strlen(suffix);
    if (output_length > SIZE_MAX - suffix_length - 1) {
      return -EOVERFLOW;
    }
    char *temporary = malloc(output_length + suffix_length + 1);
    if (temporary == NULL) {
      return -ENOMEM;
    }
    memcpy(temporary, output_path, output_length);
    memcpy(temporary + output_length, suffix, suffix_length + 1);
    int descriptor = mkstemp(temporary);
    if (descriptor < 0) {
      int status = -errno;
      free(temporary);
      return status;
    }
    if (close(descriptor) != 0) {
      int status = -errno;
      unlink(temporary);
      free(temporary);
      return status;
    }

    int32_t emit_product = product == 4 ? 3 : 2;
    char *triple = api->get_default_target_triple();
    if (triple == NULL) {
      unlink(temporary);
      free(temporary);
      return -ENOENT;
    }
    void *target = NULL;
    char *target_error = NULL;
    int target_status = api->get_target_from_triple(triple, &target, &target_error);
    if (target_status != 0 || target == NULL) {
      if (target_error != NULL) {
        api->dispose_message(target_error);
      }
      api->dispose_message(triple);
      unlink(temporary);
      free(temporary);
      return -ENOENT;
    }
    void *options = api->create_target_machine_options();
    void *machine = options == NULL ? NULL : api->create_target_machine_with_options(target, triple, options);
    if (options != NULL) {
      api->dispose_target_machine_options(options);
    }
    api->dispose_message(triple);
    if (machine == NULL) {
      unlink(temporary);
      free(temporary);
      return -ENOMEM;
    }
    char *emit_error = NULL;
    int emit_status = api->target_machine_emit_to_file(machine, module, temporary, emit_product == 2 ? 0 : 1,
                                                       &emit_error);
    if (emit_error != NULL) {
      api->dispose_message(emit_error);
    }
    api->dispose_target_machine(machine);
    if (emit_status != 0) {
      unlink(temporary);
      free(temporary);
      return -EIO;
    }

    int32_t link_status = product == 6 ? tn_selfhost_link_node_native(temporary, output_path, returns_void)
                                       : tn_selfhost_link_native(temporary, output_path, product, returns_void);
    unlink(temporary);
    free(temporary);
    return link_status;
  }
  char *triple = api->get_default_target_triple();
  if (triple == NULL) {
    return -ENOENT;
  }
  void *target = NULL;
  char *target_error = NULL;
  int target_status = api->get_target_from_triple(triple, &target, &target_error);
  if (target_status != 0 || target == NULL) {
    if (target_error != NULL) {
      api->dispose_message(target_error);
    }
    api->dispose_message(triple);
    return -ENOENT;
  }
  void *options = api->create_target_machine_options();
  void *machine = options == NULL ? NULL : api->create_target_machine_with_options(target, triple, options);
  if (options != NULL) {
    api->dispose_target_machine_options(options);
  }
  api->dispose_message(triple);
  if (machine == NULL) {
    return -ENOMEM;
  }
  char *emit_error = NULL;
  int emit_status = api->target_machine_emit_to_file(machine, module, output_path, product == 2 ? 0 : 1, &emit_error);
  if (emit_error != NULL) {
    api->dispose_message(emit_error);
  }
  api->dispose_target_machine(machine);
  return emit_status == 0 ? 0 : -EIO;
}

int32_t tn_selfhost_llvm_emit_i32_return(const char *output_path, const char *module_name,
                                         const char *function_name, int32_t value) {
  if (output_path == NULL || module_name == NULL || function_name == NULL) {
    return -EINVAL;
  }
  pthread_once(&tn_selfhost_llvm_api_once, tn_selfhost_llvm_load_api_once);
  tn_selfhost_llvm_api *api = &tn_selfhost_llvm_api_state;
  if (api->context_create == NULL || api->module_create == NULL || api->int32_type == NULL ||
      api->function_type == NULL || api->add_function == NULL || api->append_basic_block == NULL ||
      api->create_builder == NULL || api->position_builder == NULL || api->const_int == NULL ||
      api->build_ret == NULL || api->dispose_builder == NULL || api->verify_module == NULL ||
      api->print_module == NULL || api->dispose_message == NULL || api->module_dispose == NULL ||
      api->context_dispose == NULL) {
    return -ENOSYS;
  }
  void *context = api->context_create();
  if (context == NULL) {
    return -ENOMEM;
  }
  void *module = api->module_create(module_name, context);
  if (module == NULL) {
    api->context_dispose(context);
    return -ENOMEM;
  }
  void *integer = api->int32_type(context);
  void *function_type = api->function_type(integer, NULL, 0, 0);
  void *function = function_type == NULL ? NULL : api->add_function(module, function_name, function_type);
  void *block = function == NULL ? NULL : api->append_basic_block(context, function, "entry");
  void *builder = block == NULL ? NULL : api->create_builder(context);
  if (builder == NULL) {
    api->module_dispose(module);
    api->context_dispose(context);
    return -ENOMEM;
  }
  api->position_builder(builder, block);
  void *constant = api->const_int(integer, (unsigned long long)(uint32_t)value, 1);
  api->build_ret(builder, constant);
  api->dispose_builder(builder);
  char *verification_message = NULL;
  int verification = api->verify_module(module, 2, &verification_message);
  if (verification_message != NULL) {
    api->dispose_message(verification_message);
  }
  if (verification != 0) {
    api->module_dispose(module);
    api->context_dispose(context);
    return -EINVAL;
  }
  char *ir = api->print_module(module);
  int status = ir == NULL ? -ENOMEM : tn_selfhost_write(output_path, (const uint8_t *)ir, strlen(ir));
  if (ir != NULL) {
    api->dispose_message(ir);
  }
  api->module_dispose(module);
  api->context_dispose(context);
  return status;
}

typedef struct {
  int32_t kind;
  int32_t value;
} tn_selfhost_i32_operation;

static int32_t tn_selfhost_eval_i32_program_with_base(const uint8_t *operations, size_t count, int32_t *value,
                                                      int32_t argument_base, const int32_t *parameters,
                                                      size_t parameter_count) {
  if (value == NULL || (operations == NULL && count != 0)) {
    return -EINVAL;
  }
  if (argument_base < 0) {
    return -EINVAL;
  }
  if (count == 0 || count > 4096) {
    return -E2BIG;
  }
  int32_t *stack = calloc(count, sizeof(*stack));
  if (stack == NULL) {
    return -ENOMEM;
  }
  size_t depth = 0;
  int32_t status = 0;
  for (size_t index = 0; index < count; ++index) {
    tn_selfhost_i32_operation operation;
    memcpy(&operation, operations + index * sizeof(operation), sizeof(operation));
    if (operation.kind == 0) {
      stack[depth++] = operation.value;
      continue;
    }
    if (operation.kind == 1) {
      if (depth == 0 || stack[depth - 1] == INT32_MIN) {
        status = -ERANGE;
        break;
      }
      stack[depth - 1] = -stack[depth - 1];
      continue;
    }
    if (operation.kind == 7) {
      const int32_t argument_count = tn_process_argc();
      stack[depth++] = argument_count > argument_base ? argument_count - argument_base : 0;
      continue;
    }
    if (operation.kind == 11) {
      if (depth < 3) {
        status = -EINVAL;
        break;
      }
      const int32_t else_value = stack[--depth];
      const int32_t then_value = stack[--depth];
      const int32_t condition = stack[--depth];
      stack[depth++] = condition != 0 ? then_value : else_value;
      continue;
    }
    if (operation.kind >= 12 && operation.kind <= 17) {
      if (depth < 2) {
        status = -EINVAL;
        break;
      }
      const int32_t right = stack[--depth];
      const int32_t left = stack[--depth];
      int32_t result = 0;
      if (operation.kind == 12) {
        result = left == right;
      } else if (operation.kind == 13) {
        result = left != right;
      } else if (operation.kind == 14) {
        result = left < right;
      } else if (operation.kind == 15) {
        result = left <= right;
      } else if (operation.kind == 16) {
        result = left > right;
      } else {
        result = left >= right;
      }
      stack[depth++] = result;
      continue;
    }
    if (operation.kind == 8) {
      if (parameters == NULL || operation.value < 0 || (size_t)operation.value >= parameter_count) {
        status = -EINVAL;
        break;
      }
      stack[depth++] = parameters[operation.value];
      continue;
    }
    if (operation.kind < 2 || operation.kind > 6 || depth < 2) {
      status = -EINVAL;
      break;
    }
    int32_t right = stack[--depth];
    int32_t left = stack[--depth];
    int64_t wide = 0;
    if (operation.kind == 2) {
      wide = (int64_t)left + (int64_t)right;
      if (wide < INT32_MIN || wide > INT32_MAX) {
        status = -ERANGE;
        break;
      }
      stack[depth++] = (int32_t)wide;
    } else if (operation.kind == 3) {
      wide = (int64_t)left - (int64_t)right;
      if (wide < INT32_MIN || wide > INT32_MAX) {
        status = -ERANGE;
        break;
      }
      stack[depth++] = (int32_t)wide;
    } else if (operation.kind == 4) {
      wide = (int64_t)left * (int64_t)right;
      if (wide < INT32_MIN || wide > INT32_MAX) {
        status = -ERANGE;
        break;
      }
      stack[depth++] = (int32_t)wide;
    } else if (operation.kind == 5) {
      if (right == 0 || (left == INT32_MIN && right == -1)) {
        status = -EDOM;
        break;
      }
      stack[depth++] = left / right;
    } else {
      if (right == 0 || (left == INT32_MIN && right == -1)) {
        status = -EDOM;
        break;
      }
      stack[depth++] = left % right;
    }
  }
  if (status == 0 && depth != 1) {
    status = -EINVAL;
  }
  if (status == 0) {
    *value = stack[0];
  }
  free(stack);
  return status;
}

int32_t tn_selfhost_eval_i32_program(const uint8_t *operations, size_t count, int32_t *value) {
  return tn_selfhost_eval_i32_program_with_base(operations, count, value, 0, NULL, 0);
}

int32_t tn_selfhost_eval_i32_program_for_cli(const uint8_t *operations, size_t count, int32_t *value,
                                             int32_t argument_base) {
  return tn_selfhost_eval_i32_program_with_base(operations, count, value, argument_base, NULL, 0);
}

int32_t tn_selfhost_eval_i32_program_with_parameters(const uint8_t *operations, size_t count,
                                                     const int32_t *parameters, size_t parameter_count,
                                                     int32_t *value) {
  return tn_selfhost_eval_i32_program_with_base(operations, count, value, 0, parameters, parameter_count);
}

int32_t tn_selfhost_eval_i32_constant(int32_t kind, int32_t left, int32_t right, int32_t *value) {
  if (value == NULL || kind < 2 || kind > 6) {
    return -EINVAL;
  }
  if ((kind == 5 || kind == 6) &&
      (right == 0 || (left == INT32_MIN && right == -1))) {
    return kind == 5 ? -EDOM : -EDOM;
  }
  int64_t wide = 0;
  if (kind == 2) {
    wide = (int64_t)left + (int64_t)right;
  } else if (kind == 3) {
    wide = (int64_t)left - (int64_t)right;
  } else if (kind == 4) {
    wide = (int64_t)left * (int64_t)right;
  } else if (kind == 5) {
    wide = left / right;
  } else {
    wide = left % right;
  }
  if (wide < INT32_MIN || wide > INT32_MAX) {
    return -ERANGE;
  }
  *value = (int32_t)wide;
  return 0;
}

static int32_t tn_selfhost_llvm_emit_i32_program_product_with_parameters(const char *output_path,
                                                                         const char *module_name,
                                                                         const char *function_name,
                                                                         const uint8_t *operations, size_t count,
                                                                         size_t parameter_count, int32_t product) {
  if (output_path == NULL || module_name == NULL || function_name == NULL ||
      (operations == NULL && count != 0)) {
    return -EINVAL;
  }
  if (count == 0 || count > 512 || parameter_count > 32) {
    return -E2BIG;
  }
  pthread_once(&tn_selfhost_llvm_api_once, tn_selfhost_llvm_load_api_once);
  tn_selfhost_llvm_api *api = &tn_selfhost_llvm_api_state;
  if (api->context_create == NULL || api->module_create == NULL || api->int32_type == NULL ||
      api->function_type == NULL || api->add_function == NULL || api->get_param == NULL ||
      api->append_basic_block == NULL ||
      api->create_builder == NULL || api->position_builder == NULL || api->const_int == NULL ||
      api->build_add == NULL || api->build_sub == NULL || api->build_mul == NULL ||
      api->build_sdiv == NULL || api->build_srem == NULL || api->build_neg == NULL ||
      api->build_icmp == NULL || api->build_select == NULL || api->build_zext == NULL ||
      api->build_call2 == NULL ||
      api->build_ret == NULL || api->dispose_builder == NULL ||
      api->verify_module == NULL ||
      api->print_module == NULL || api->dispose_message == NULL || api->module_dispose == NULL ||
      api->context_dispose == NULL) {
    return -ENOSYS;
  }
  void *context = api->context_create();
  if (context == NULL) {
    return -ENOMEM;
  }
  void *module = api->module_create(module_name, context);
  if (module == NULL) {
    api->context_dispose(context);
    return -ENOMEM;
  }
  void *integer = api->int32_type(context);
  void *parameter_types[32];
  for (size_t index = 0; index < parameter_count; ++index) {
    parameter_types[index] = integer;
  }
  void *function_type = api->function_type(integer, parameter_count == 0 ? NULL : parameter_types,
                                           (unsigned)parameter_count, 0);
  void *function = function_type == NULL ? NULL : api->add_function(module, function_name, function_type);
  void *zero_function_type = integer == NULL ? NULL : api->function_type(integer, NULL, 0, 0);
  void *argument_count_function = zero_function_type == NULL
                                      ? NULL
                                      : api->add_function(module, "tn_process_argc", zero_function_type);
  void *block = function == NULL ? NULL : api->append_basic_block(context, function, "entry");
  void *builder = block == NULL ? NULL : api->create_builder(context);
  if (builder == NULL || argument_count_function == NULL) {
    api->module_dispose(module);
    api->context_dispose(context);
    return -ENOMEM;
  }
  api->position_builder(builder, block);
  void *parameter_values[32];
  for (size_t index = 0; index < parameter_count; ++index) {
    parameter_values[index] = api->get_param(function, (unsigned)index);
    if (parameter_values[index] == NULL) {
      api->dispose_builder(builder);
      api->module_dispose(module);
      api->context_dispose(context);
      return -EINVAL;
    }
  }
  void *values[512];
  int32_t known_values[512];
  uint8_t known[512];
  size_t depth = 0;
  int32_t status = 0;
  for (size_t index = 0; index < count; ++index) {
    tn_selfhost_i32_operation operation;
    memcpy(&operation, operations + index * sizeof(operation), sizeof(operation));
    if (operation.kind == 0) {
      values[depth++] = api->const_int(integer, (unsigned long long)(uint32_t)operation.value, 1);
      known[depth - 1] = 1;
      known_values[depth - 1] = operation.value;
      continue;
    }
    if (operation.kind == 1) {
      if (depth == 0) {
        status = -EINVAL;
        break;
      }
      if (known[depth - 1] != 0) {
        if (known_values[depth - 1] == INT32_MIN) {
          status = -ERANGE;
          break;
        }
        known_values[depth - 1] = -known_values[depth - 1];
      }
      values[depth - 1] = api->build_neg(builder, values[depth - 1], "neg");
      if (values[depth - 1] == NULL) {
        status = -EINVAL;
        break;
      }
      continue;
    }
    if (operation.kind == 7) {
      void *argument_count = api->build_call2(builder, zero_function_type, argument_count_function, NULL, 0,
                                              "argument_count");
      if (argument_count == NULL) {
        status = -EINVAL;
        break;
      }
      values[depth++] = argument_count;
      known[depth - 1] = 0;
      continue;
    }
    if (operation.kind == 11) {
      if (depth < 3) {
        status = -EINVAL;
        break;
      }
      const size_t else_index = --depth;
      void *else_value = values[else_index];
      const uint8_t else_known = known[else_index];
      const int32_t else_constant = known_values[else_index];
      const size_t then_index = --depth;
      void *then_value = values[then_index];
      const uint8_t then_known = known[then_index];
      const int32_t then_constant = known_values[then_index];
      const size_t condition_index = --depth;
      void *condition = values[condition_index];
      const uint8_t condition_known = known[condition_index];
      const int32_t condition_constant = known_values[condition_index];
      void *zero = api->const_int(integer, 0, 1);
      void *predicate = zero == NULL ? NULL : api->build_icmp(builder, 33, condition, zero, "condition");
      void *selected = predicate == NULL ? NULL : api->build_select(builder, predicate, then_value, else_value, "select");
      if (selected == NULL) {
        status = -EINVAL;
        break;
      }
      values[depth] = selected;
      known[depth] = 0;
      known_values[depth] = 0;
      if (condition_known != 0) {
        if (condition_constant != 0 && then_known != 0) {
          known[depth] = 1;
          known_values[depth] = then_constant;
        } else if (condition_constant == 0 && else_known != 0) {
          known[depth] = 1;
          known_values[depth] = else_constant;
        }
      }
      depth += 1;
      continue;
    }
    if (operation.kind >= 12 && operation.kind <= 17) {
      if (depth < 2) {
        status = -EINVAL;
        break;
      }
      const size_t right_index = --depth;
      void *right = values[right_index];
      const uint8_t right_known = known[right_index];
      const int32_t right_value = known_values[right_index];
      const size_t left_index = --depth;
      void *left = values[left_index];
      const uint8_t left_known = known[left_index];
      const int32_t left_value = known_values[left_index];
      int predicate_kind = 32;
      if (operation.kind == 13) {
        predicate_kind = 33;
      } else if (operation.kind == 14) {
        predicate_kind = 40;
      } else if (operation.kind == 15) {
        predicate_kind = 41;
      } else if (operation.kind == 16) {
        predicate_kind = 38;
      } else if (operation.kind == 17) {
        predicate_kind = 39;
      }
      void *comparison = api->build_icmp(builder, predicate_kind, left, right, "comparison");
      void *result = comparison == NULL ? NULL : api->build_zext(builder, comparison, integer, "comparison_i32");
      if (result == NULL) {
        status = -EINVAL;
        break;
      }
      values[depth] = result;
      known[depth] = 0;
      known_values[depth] = 0;
      if (left_known != 0 && right_known != 0) {
        int32_t result_value = 0;
        if (operation.kind == 12) {
          result_value = left_value == right_value;
        } else if (operation.kind == 13) {
          result_value = left_value != right_value;
        } else if (operation.kind == 14) {
          result_value = left_value < right_value;
        } else if (operation.kind == 15) {
          result_value = left_value <= right_value;
        } else if (operation.kind == 16) {
          result_value = left_value > right_value;
        } else {
          result_value = left_value >= right_value;
        }
        known[depth] = 1;
        known_values[depth] = result_value;
        }
      depth += 1;
      continue;
    }
    if (operation.kind == 8) {
      if (operation.value < 0 || (size_t)operation.value >= parameter_count) {
        status = -EINVAL;
        break;
      }
      values[depth] = parameter_values[operation.value];
      known[depth] = 0;
      known_values[depth] = 0;
      depth += 1;
      continue;
    }
    if (operation.kind < 2 || operation.kind > 6 || depth < 2) {
      status = -EINVAL;
      break;
    }
    const size_t right_index = depth - 1;
    void *right = values[right_index];
    const uint8_t right_known = known[right_index];
    const int32_t right_value = known_values[right_index];
    depth -= 1;
    const size_t left_index = depth - 1;
    void *left = values[left_index];
    const uint8_t left_known = known[left_index];
    const int32_t left_value = known_values[left_index];
    depth -= 1;
    void *result = NULL;
    uint8_t result_known = 0;
    int32_t result_value = 0;
    if (left_known != 0 && right_known != 0) {
      int64_t wide = 0;
      if (operation.kind == 2) {
        wide = (int64_t)left_value + (int64_t)right_value;
      } else if (operation.kind == 3) {
        wide = (int64_t)left_value - (int64_t)right_value;
      } else if (operation.kind == 4) {
        wide = (int64_t)left_value * (int64_t)right_value;
      } else if (right_value == 0 || (left_value == INT32_MIN && right_value == -1)) {
        status = -EDOM;
        break;
      } else if (operation.kind == 5) {
        wide = left_value / right_value;
      } else {
        wide = left_value % right_value;
      }
      if (wide < INT32_MIN || wide > INT32_MAX) {
        status = -ERANGE;
        break;
      }
      result_known = 1;
      result_value = (int32_t)wide;
    }
    if (operation.kind == 2) {
      result = api->build_add(builder, left, right, "add");
    } else if (operation.kind == 3) {
      result = api->build_sub(builder, left, right, "sub");
    } else if (operation.kind == 4) {
      result = api->build_mul(builder, left, right, "mul");
    } else if (operation.kind == 5) {
      result = api->build_sdiv(builder, left, right, "sdiv");
    } else {
      result = api->build_srem(builder, left, right, "srem");
    }
    if (result == NULL) {
      status = -EINVAL;
      break;
    }
    values[depth++] = result;
    known[depth - 1] = result_known;
    known_values[depth - 1] = result_value;
  }
  if (status == 0 && depth == 1) {
    if (api->build_ret(builder, values[0]) == NULL) {
      status = -EINVAL;
    }
  } else if (status == 0) {
    status = -EINVAL;
  }
  api->dispose_builder(builder);
  if (status == 0) {
    char *verification_message = NULL;
    if (api->verify_module(module, 2, &verification_message) != 0) {
      status = -EINVAL;
    }
    if (verification_message != NULL) {
      api->dispose_message(verification_message);
    }
  }
  if (status == 0) {
    status = tn_selfhost_llvm_write_product(api, module, output_path, product, 0);
  }
  api->module_dispose(module);
  api->context_dispose(context);
  return status;
}

int32_t tn_selfhost_llvm_emit_i32_program_product(const char *output_path, const char *module_name,
                                                  const char *function_name,
                                                  const uint8_t *operations, size_t count, int32_t product) {
  return tn_selfhost_llvm_emit_i32_program_product_with_parameters(output_path, module_name, function_name,
                                                                    operations, count, 0, product);
}

int32_t tn_selfhost_llvm_emit_i32_program(const char *output_path, const char *module_name,
                                          const char *function_name,
                                          const uint8_t *operations, size_t count) {
  return tn_selfhost_llvm_emit_i32_program_product(output_path, module_name, function_name, operations, count, 0);
}

int32_t tn_selfhost_llvm_emit_i32_program_with_parameters_product(const char *output_path,
                                                                   const char *module_name,
                                                                   const char *function_name,
                                                                   const uint8_t *operations, size_t count,
                                                                   size_t parameter_count, int32_t product) {
  return tn_selfhost_llvm_emit_i32_program_product_with_parameters(output_path, module_name, function_name,
                                                                    operations, count, parameter_count, product);
}

int32_t tn_selfhost_llvm_emit_i32_program_with_parameters(const char *output_path,
                                                          const char *module_name,
                                                          const char *function_name,
                                                          const uint8_t *operations, size_t count,
                                                          size_t parameter_count) {
  return tn_selfhost_llvm_emit_i32_program_with_parameters_product(output_path, module_name, function_name,
                                                                     operations, count, parameter_count, 0);
}

#include "selfhost_module.c"

int32_t tn_selfhost_llvm_emit_void_program_product(const char *output_path, const char *module_name,
                                                   const char *function_name, int32_t product) {
  if (output_path == NULL || module_name == NULL || function_name == NULL) {
    return -EINVAL;
  }
  pthread_once(&tn_selfhost_llvm_api_once, tn_selfhost_llvm_load_api_once);
  tn_selfhost_llvm_api *api = &tn_selfhost_llvm_api_state;
  if (api->context_create == NULL || api->module_create == NULL || api->void_type == NULL ||
      api->function_type == NULL || api->add_function == NULL || api->append_basic_block == NULL ||
      api->create_builder == NULL || api->position_builder == NULL || api->build_ret_void == NULL ||
      api->dispose_builder == NULL || api->verify_module == NULL || api->print_module == NULL ||
      api->dispose_message == NULL || api->module_dispose == NULL || api->context_dispose == NULL) {
    return -ENOSYS;
  }
  void *context = api->context_create();
  if (context == NULL) {
    return -ENOMEM;
  }
  void *module = api->module_create(module_name, context);
  if (module == NULL) {
    api->context_dispose(context);
    return -ENOMEM;
  }
  void *void_value = api->void_type(context);
  void *function_type = void_value == NULL ? NULL : api->function_type(void_value, NULL, 0, 0);
  void *function = function_type == NULL ? NULL : api->add_function(module, function_name, function_type);
  void *block = function == NULL ? NULL : api->append_basic_block(context, function, "entry");
  void *builder = block == NULL ? NULL : api->create_builder(context);
  if (builder == NULL) {
    api->module_dispose(module);
    api->context_dispose(context);
    return -ENOMEM;
  }
  api->position_builder(builder, block);
  int32_t status = api->build_ret_void(builder) == NULL ? -EINVAL : 0;
  api->dispose_builder(builder);
  if (status == 0) {
    char *verification_message = NULL;
    if (api->verify_module(module, 2, &verification_message) != 0) {
      status = -EINVAL;
    }
    if (verification_message != NULL) {
      api->dispose_message(verification_message);
    }
  }
  if (status == 0) {
    status = tn_selfhost_llvm_write_product(api, module, output_path, product, 1);
  }
  api->module_dispose(module);
  api->context_dispose(context);
  return status;
}

int32_t tn_selfhost_llvm_emit_void_program(const char *output_path, const char *module_name,
                                            const char *function_name) {
  return tn_selfhost_llvm_emit_void_program_product(output_path, module_name, function_name, 0);
}

int32_t tn_selfhost_llvm_c_api_smoke(void) {
  typedef void *(*llvm_context_create)(void);
  typedef void *(*llvm_module_create)(const char *, void *);
  typedef void (*llvm_module_dispose)(void *);
  typedef void (*llvm_context_dispose)(void *);
  static const char *const candidates[] = {"/opt/homebrew/opt/llvm/lib/libLLVM.dylib", "libLLVM.dylib",
                                           "libLLVM.so.22", "libLLVM-22.so", "libLLVM.so"};
  for (size_t index = 0; index < sizeof(candidates) / sizeof(candidates[0]); ++index) {
    void *library = dlopen(candidates[index], RTLD_LAZY | RTLD_LOCAL);
    if (library == NULL) {
      continue;
    }
    union {
      void *object;
      llvm_context_create function;
    } create_context = {.object = dlsym(library, "LLVMContextCreate")};
    union {
      void *object;
      llvm_module_create function;
    } create_module = {.object = dlsym(library, "LLVMModuleCreateWithNameInContext")};
    union {
      void *object;
      llvm_module_dispose function;
    } dispose_module = {.object = dlsym(library, "LLVMDisposeModule")};
    union {
      void *object;
      llvm_context_dispose function;
    } dispose_context = {.object = dlsym(library, "LLVMContextDispose")};
    if (create_context.function == NULL || create_module.function == NULL || dispose_module.function == NULL ||
        dispose_context.function == NULL) {
      dlclose(library);
      continue;
    }
    void *context = create_context.function();
    void *module = context == NULL ? NULL : create_module.function("typenative-selfhost", context);
    int32_t status = module == NULL ? -ENOMEM : 0;
    if (module != NULL) {
      dispose_module.function(module);
    }
    if (context != NULL) {
      dispose_context.function(context);
    }
    dlclose(library);
    return status;
  }
  return -ENOENT;
}

void *tn_library_open(const char *path) { return dlopen(path, RTLD_NOW | RTLD_LOCAL); }
void *tn_library_symbol(void *handle, const char *name) {
  return handle == NULL ? NULL : dlsym(handle, name);
}
int tn_library_close(void *handle) { return handle == NULL ? 0 : dlclose(handle); }
