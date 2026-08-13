#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <poll.h>
#include <time.h>
#include <unistd.h>

typedef struct RedisEntry {
  char *key;
  char *value;
  uint64_t expires_ns;
  struct RedisEntry *next;
} RedisEntry;

typedef struct RedisClient {
  int handle;
  struct RedisClient *next;
} RedisClient;

static RedisEntry *redis_entries;
static pthread_mutex_t redis_mutex = PTHREAD_MUTEX_INITIALIZER;
static _Atomic uint64_t redis_clock_override;
static _Atomic int redis_clock_is_overridden;
static _Atomic int redis_shutdown_requested;
static _Atomic int redis_server_handle = -1;
static pthread_mutex_t redis_clients_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t redis_clients_condition = PTHREAD_COND_INITIALIZER;
static RedisClient *redis_clients;
static size_t redis_active_clients;

static void clear_entries_locked(void) {
  RedisEntry *entry = redis_entries;
  redis_entries = NULL;
  while (entry != NULL) {
    RedisEntry *next = entry->next;
    free(entry->key);
    free(entry->value);
    free(entry);
    entry = next;
  }
}

static uint64_t redis_now_ns(void) {
  if (atomic_load(&redis_clock_is_overridden)) {
    return atomic_load(&redis_clock_override);
  }
  struct timespec value;
  if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
    return 0;
  }
  return (uint64_t)value.tv_sec * UINT64_C(1000000000) + (uint64_t)value.tv_nsec;
}

void tn_redis_set_clock(uint64_t monotonic_ns) {
  atomic_store(&redis_clock_override, monotonic_ns);
  atomic_store(&redis_clock_is_overridden, 1);
}

void tn_redis_clear_clock(void) { atomic_store(&redis_clock_is_overridden, 0); }

void tn_redis_stop(void) {
  atomic_store(&redis_shutdown_requested, 1);
  int server = atomic_load(&redis_server_handle);
  if (server >= 0) {
    shutdown(server, SHUT_RDWR);
  }
  pthread_mutex_lock(&redis_clients_mutex);
  for (RedisClient *client = redis_clients; client != NULL; client = client->next) {
    shutdown(client->handle, SHUT_RDWR);
  }
  pthread_mutex_unlock(&redis_clients_mutex);
}

static int send_all(int client, const void *bytes, size_t length) {
  const char *cursor = bytes;
  while (length != 0) {
    ssize_t written = send(client, cursor, length, 0);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written <= 0) {
      return -1;
    }
    cursor += written;
    length -= (size_t)written;
  }
  return 0;
}

static int send_text(int client, const char *text) {
  return send_all(client, text, strlen(text));
}

static int send_bulk(int client, const char *value, size_t length) {
  char header[64];
  int written = snprintf(header, sizeof(header), "$%zu\r\n", length);
  if (written < 0 || send_all(client, header, (size_t)written) != 0 ||
      send_all(client, value, length) != 0 || send_text(client, "\r\n") != 0) {
    return -1;
  }
  return 0;
}

static int read_exact(int client, char *buffer, size_t length) {
  size_t offset = 0;
  while (offset < length) {
    ssize_t received = recv(client, buffer + offset, length - offset, 0);
    if (received < 0 && errno == EINTR) {
      continue;
    }
    if (received <= 0) {
      return -1;
    }
    offset += (size_t)received;
  }
  return 0;
}

static int read_line(int client, char *buffer, size_t capacity) {
  size_t length = 0;
  while (length + 1 < capacity) {
    char byte;
    ssize_t received = recv(client, &byte, 1, 0);
    if (received < 0 && errno == EINTR) {
      continue;
    }
    if (received <= 0) {
      return -1;
    }
    if (byte == '\n') {
      if (length != 0 && buffer[length - 1] == '\r') {
        buffer[length - 1] = '\0';
        return 0;
      }
      return -1;
    }
    buffer[length++] = byte;
  }
  return -1;
}

static int parse_size(const char *text, size_t maximum, size_t *value) {
  if (text == NULL || *text == '\0' || *text == '-') {
    return -1;
  }
  errno = 0;
  char *end = NULL;
  unsigned long long parsed = strtoull(text, &end, 10);
  if (errno == ERANGE || end == text || *end != '\0' || parsed > maximum) {
    return -1;
  }
  *value = (size_t)parsed;
  return 0;
}

static int parse_i64(const char *text, int64_t *value) {
  if (text == NULL || *text == '\0') {
    return -1;
  }
  errno = 0;
  char *end = NULL;
  long long parsed = strtoll(text, &end, 10);
  if (errno == ERANGE || end == text || *end != '\0') {
    return -1;
  }
  *value = (int64_t)parsed;
  return 0;
}

static void free_command(char **arguments, size_t count) {
  for (size_t index = 0; index < count; ++index) {
    free(arguments[index]);
    arguments[index] = NULL;
  }
}

static int read_command(int client, char **arguments, size_t *count) {
  char line[128];
  if (read_line(client, line, sizeof(line)) != 0 || line[0] != '*') {
    return -1;
  }
  size_t parsed_count = 0;
  if (parse_size(line + 1, 64, &parsed_count) != 0 || parsed_count == 0) {
    return -1;
  }
  *count = (size_t)parsed_count;
  for (size_t index = 0; index < *count; ++index) {
    if (read_line(client, line, sizeof(line)) != 0 || line[0] != '$') {
      free_command(arguments, index);
      return -1;
    }
    size_t length = 0;
    if (parse_size(line + 1, 64 * 1024 * 1024, &length) != 0) {
      free_command(arguments, index);
      return -1;
    }
    arguments[index] = malloc(length + 1);
    if (arguments[index] == NULL || read_exact(client, arguments[index], length) != 0) {
      free(arguments[index]);
      arguments[index] = NULL;
      free_command(arguments, index);
      return -1;
    }
    arguments[index][length] = '\0';
    char terminator[2];
    if (read_exact(client, terminator, sizeof(terminator)) != 0 || terminator[0] != '\r' ||
        terminator[1] != '\n') {
      free_command(arguments, index + 1);
      return -1;
    }
  }
  return 0;
}

static int equal_command(const char *left, const char *right) {
  while (*left != '\0' && *right != '\0') {
    char a = *left >= 'a' && *left <= 'z' ? (char)(*left - 'a' + 'A') : *left;
    if (a != *right) {
      return 0;
    }
    ++left;
    ++right;
  }
  return *left == '\0' && *right == '\0';
}

static int expiration_after_seconds(const char *text, uint64_t *expires) {
  int64_t seconds = 0;
  if (parse_i64(text, &seconds) != 0 || seconds < 0 ||
      (uint64_t)seconds > (UINT64_MAX - redis_now_ns()) / UINT64_C(1000000000)) {
    return -1;
  }
  *expires = redis_now_ns() + (uint64_t)seconds * UINT64_C(1000000000);
  return 0;
}

static RedisEntry *find_entry_locked(const char *key, RedisEntry **previous) {
  RedisEntry *prior = NULL;
  RedisEntry *entry = redis_entries;
  uint64_t now = redis_now_ns();
  while (entry != NULL) {
    RedisEntry *next = entry->next;
    if (entry->expires_ns != 0 && entry->expires_ns <= now) {
      if (prior == NULL) {
        redis_entries = next;
      } else {
        prior->next = next;
      }
      free(entry->key);
      free(entry->value);
      free(entry);
      entry = next;
      continue;
    }
    if (strcmp(entry->key, key) == 0) {
      if (previous != NULL) {
        *previous = prior;
      }
      return entry;
    }
    prior = entry;
    entry = next;
  }
  if (previous != NULL) {
    *previous = NULL;
  }
  return NULL;
}

static int set_entry_locked(const char *key, const char *value, uint64_t expires_ns) {
  RedisEntry *entry = find_entry_locked(key, NULL);
  int inserted = 0;
  if (entry == NULL) {
    entry = calloc(1, sizeof(*entry));
    if (entry == NULL) {
      return -1;
    }
    entry->key = strdup(key);
    if (entry->key == NULL) {
      free(entry);
      return -1;
    }
    inserted = 1;
  }
  char *copy = strdup(value);
  if (copy == NULL) {
    if (inserted) {
      free(entry->key);
      free(entry);
    }
    free(copy);
    return -1;
  }
  if (inserted) {
    entry->next = redis_entries;
    redis_entries = entry;
  }
  free(entry->value);
  entry->value = copy;
  entry->expires_ns = expires_ns;
  return 0;
}

static int handle_command(int client, char **arguments, size_t count) {
  if (equal_command(arguments[0], "PING")) {
    return count == 2 ? send_bulk(client, arguments[1], strlen(arguments[1]))
                      : send_text(client, "+PONG\r\n");
  }
  if (equal_command(arguments[0], "ECHO") && count == 2) {
    return send_bulk(client, arguments[1], strlen(arguments[1]));
  }
  if (equal_command(arguments[0], "SET") && count >= 3) {
    uint64_t expires = 0;
    if (count != 3 &&
        (count != 5 || !equal_command(arguments[3], "EX") ||
         expiration_after_seconds(arguments[4], &expires) != 0)) {
      return send_text(client, "-ERR syntax error\r\n");
    }
    pthread_mutex_lock(&redis_mutex);
    int result = set_entry_locked(arguments[1], arguments[2], expires);
    pthread_mutex_unlock(&redis_mutex);
    return result == 0 ? send_text(client, "+OK\r\n") : send_text(client, "-ERR allocation\r\n");
  }
  if (equal_command(arguments[0], "GET") && count == 2) {
    pthread_mutex_lock(&redis_mutex);
    RedisEntry *entry = find_entry_locked(arguments[1], NULL);
    int result = entry == NULL ? send_text(client, "$-1\r\n")
                               : send_bulk(client, entry->value, strlen(entry->value));
    pthread_mutex_unlock(&redis_mutex);
    return result;
  }
  if ((equal_command(arguments[0], "EXISTS") || equal_command(arguments[0], "DEL")) && count == 2) {
    pthread_mutex_lock(&redis_mutex);
    RedisEntry *previous = NULL;
    RedisEntry *entry = find_entry_locked(arguments[1], &previous);
    long result = entry == NULL ? 0 : 1;
    if (equal_command(arguments[0], "DEL") && entry != NULL) {
      if (previous == NULL) {
        redis_entries = entry->next;
      } else {
        previous->next = entry->next;
      }
      free(entry->key);
      free(entry->value);
      free(entry);
    }
    pthread_mutex_unlock(&redis_mutex);
    char response[64];
    int length = snprintf(response, sizeof(response), ":%ld\r\n", result);
    return length < 0 ? -1 : send_all(client, response, (size_t)length);
  }
  if (equal_command(arguments[0], "INCR") && count == 2) {
    pthread_mutex_lock(&redis_mutex);
    RedisEntry *entry = find_entry_locked(arguments[1], NULL);
    int64_t value = 0;
    if (entry != NULL && parse_i64(entry->value, &value) != 0) {
      pthread_mutex_unlock(&redis_mutex);
      return send_text(client, "-ERR value is not an integer\r\n");
    }
    if (value == INT64_MAX) {
      pthread_mutex_unlock(&redis_mutex);
      return send_text(client, "-ERR increment or decrement would overflow\r\n");
    }
    ++value;
    char text[64];
    snprintf(text, sizeof(text), "%lld", (long long)value);
    int result = set_entry_locked(arguments[1], text, entry == NULL ? 0 : entry->expires_ns);
    pthread_mutex_unlock(&redis_mutex);
    if (result != 0) {
      return send_text(client, "-ERR allocation\r\n");
    }
    char response[96];
    int length = snprintf(response, sizeof(response), ":%" PRId64 "\r\n", value);
    return length < 0 ? -1 : send_all(client, response, (size_t)length);
  }
  if (equal_command(arguments[0], "EXPIRE") && count == 3) {
    uint64_t expires = 0;
    if (expiration_after_seconds(arguments[2], &expires) != 0) {
      return send_text(client, "-ERR invalid expire time\r\n");
    }
    pthread_mutex_lock(&redis_mutex);
    RedisEntry *entry = find_entry_locked(arguments[1], NULL);
    int found = entry != NULL;
    if (entry != NULL) {
      entry->expires_ns = expires;
    }
    pthread_mutex_unlock(&redis_mutex);
    return send_text(client, found ? ":1\r\n" : ":0\r\n");
  }
  if (equal_command(arguments[0], "TTL") && count == 2) {
    pthread_mutex_lock(&redis_mutex);
    RedisEntry *entry = find_entry_locked(arguments[1], NULL);
    long long seconds = -2;
    if (entry != NULL) {
      if (entry->expires_ns == 0) {
        seconds = -1;
      } else {
        uint64_t now = redis_now_ns();
        seconds = now >= entry->expires_ns
                      ? 0
                      : (long long)((entry->expires_ns - now + UINT64_C(999999999)) /
                                    UINT64_C(1000000000));
      }
    }
    pthread_mutex_unlock(&redis_mutex);
    char response[64];
    int length = snprintf(response, sizeof(response), ":%lld\r\n", seconds);
    return length < 0 ? -1 : send_all(client, response, (size_t)length);
  }
  if (equal_command(arguments[0], "COMMAND")) {
    return send_text(client, "*0\r\n");
  }
  if (equal_command(arguments[0], "QUIT")) {
    send_text(client, "+OK\r\n");
    return 1;
  }
  return send_text(client, "-ERR unknown command\r\n");
}

static void *redis_client(void *argument) {
  RedisClient *state = argument;
  int client = state->handle;
  for (;;) {
    char *arguments[64] = {0};
    size_t count = 0;
    if (read_command(client, arguments, &count) != 0) {
      free_command(arguments, count);
      break;
    }
    int result = handle_command(client, arguments, count);
    free_command(arguments, count);
    if (result != 0) {
      break;
    }
  }
  pthread_mutex_lock(&redis_clients_mutex);
  RedisClient **cursor = &redis_clients;
  while (*cursor != NULL && *cursor != state) {
    cursor = &(*cursor)->next;
  }
  if (*cursor == state) {
    *cursor = state->next;
    redis_active_clients -= 1;
    pthread_cond_broadcast(&redis_clients_condition);
  }
  pthread_mutex_unlock(&redis_clients_mutex);
  close(client);
  free(state);
  return NULL;
}

int tn_redis_main(int32_t port) {
  atomic_store(&redis_shutdown_requested, 0);
  pthread_mutex_lock(&redis_mutex);
  clear_entries_locked();
  pthread_mutex_unlock(&redis_mutex);
  int server = socket(AF_INET, SOCK_STREAM, 0);
  if (server < 0) {
    return 1;
  }
  int reuse = 1;
  setsockopt(server, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse));
  struct sockaddr_in address = {0};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(UINT32_C(0x7f000001));
  address.sin_port = htons((uint16_t)port);
  if (bind(server, (struct sockaddr *)&address, sizeof(address)) != 0 || listen(server, 128) != 0) {
    close(server);
    return 1;
  }
  atomic_store(&redis_server_handle, server);
  for (;;) {
    if (atomic_load(&redis_shutdown_requested)) {
      break;
    }
    struct pollfd readiness = {.fd = server, .events = POLLIN};
    int polled = poll(&readiness, 1, 100);
    if (polled < 0) {
      if (errno == EINTR) {
        continue;
      }
      break;
    }
    if (polled == 0 || (readiness.revents & POLLIN) == 0) {
      continue;
    }
    int client = accept(server, NULL, NULL);
    if (client < 0) {
      if (errno == EINTR) {
        continue;
      }
      if (atomic_load(&redis_shutdown_requested)) {
        break;
      }
      close(server);
      return 1;
    }
    RedisClient *argument = malloc(sizeof(*argument));
    if (argument == NULL) {
      close(client);
      continue;
    }
    argument->handle = client;
    pthread_mutex_lock(&redis_clients_mutex);
    argument->next = redis_clients;
    redis_clients = argument;
    redis_active_clients += 1;
    pthread_mutex_unlock(&redis_clients_mutex);
    pthread_t thread;
    if (pthread_create(&thread, NULL, redis_client, argument) == 0) {
      pthread_detach(thread);
    } else {
      pthread_mutex_lock(&redis_clients_mutex);
      RedisClient **cursor = &redis_clients;
      while (*cursor != NULL && *cursor != argument) {
        cursor = &(*cursor)->next;
      }
      if (*cursor == argument) {
        *cursor = argument->next;
        redis_active_clients -= 1;
      }
      pthread_mutex_unlock(&redis_clients_mutex);
      close(client);
      free(argument);
    }
  }
  int expected = server;
  if (atomic_compare_exchange_strong(&redis_server_handle, &expected, -1)) {
    close(server);
  }
  pthread_mutex_lock(&redis_clients_mutex);
  while (redis_active_clients != 0) {
    pthread_cond_wait(&redis_clients_condition, &redis_clients_mutex);
  }
  pthread_mutex_unlock(&redis_clients_mutex);
  pthread_mutex_lock(&redis_mutex);
  clear_entries_locked();
  pthread_mutex_unlock(&redis_mutex);
  return 0;
}
