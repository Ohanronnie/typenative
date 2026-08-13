#define _POSIX_C_SOURCE 200809L

#include <arpa/inet.h>
#include <assert.h>
#include <errno.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdint.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

extern int tn_redis_main(int32_t port);
extern void tn_redis_stop(void);
extern void tn_redis_set_clock(uint64_t monotonic_ns);
extern void tn_redis_clear_clock(void);

static const int port = 6394;

static void *run_server(void *argument) {
  (void)argument;
  return (void *)(intptr_t)tn_redis_main(port);
}

static int connect_client(void) {
  int client = socket(AF_INET, SOCK_STREAM, 0);
  assert(client >= 0);
  struct sockaddr_in address = {0};
  address.sin_family = AF_INET;
  address.sin_port = htons((uint16_t)port);
  address.sin_addr.s_addr = htonl(UINT32_C(0x7f000001));
  for (size_t attempt = 0; attempt < 100; ++attempt) {
    if (connect(client, (struct sockaddr *)&address, sizeof(address)) == 0) {
      return client;
    }
    if (errno != ECONNREFUSED && errno != EINTR) {
      close(client);
      client = socket(AF_INET, SOCK_STREAM, 0);
      assert(client >= 0);
    }
    struct timespec delay = {.tv_sec = 0, .tv_nsec = 10000000};
    nanosleep(&delay, NULL);
  }
  assert(!"server did not accept a connection");
  return -1;
}

static void send_fragmented(int client, const char *bytes, size_t length) {
  for (size_t index = 0; index < length; ++index) {
    ssize_t written = send(client, bytes + index, 1, 0);
    assert(written == 1);
  }
}

static void read_response(int client, const char *expected) {
  char response[128] = {0};
  size_t length = 0;
  while (length + 1 < sizeof(response)) {
    ssize_t received = recv(client, response + length, 1, 0);
    assert(received == 1);
    length += 1;
    if (length >= 2 && response[length - 2] == '\r' && response[length - 1] == '\n') {
      break;
    }
  }
  if (strcmp(response, expected) != 0) {
    assert(!"unexpected response");
  }
}

static void stop_and_join(pthread_t thread) {
  tn_redis_stop();
  void *result = NULL;
  assert(pthread_join(thread, &result) == 0);
  assert((intptr_t)result == 0);
}

int main(void) {
  pthread_t thread;
  assert(pthread_create(&thread, NULL, run_server, NULL) == 0);
  int client = connect_client();
  const char ping[] = "*1\r\n$4\r\nPING\r\n";
  send_fragmented(client, ping, sizeof(ping) - 1);
  read_response(client, "+PONG\r\n");

  int slow = connect_client();
  const char partial[] = "*1\r\n$4\r\nPI";
  send_fragmented(slow, partial, sizeof(partial) - 1);
  tn_redis_stop();
  close(slow);
  close(client);
  void *result = NULL;
  assert(pthread_join(thread, &result) == 0);
  assert((intptr_t)result == 0);

  assert(pthread_create(&thread, NULL, run_server, NULL) == 0);
  client = connect_client();
  const char set[] = "*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n";
  send_fragmented(client, set, sizeof(set) - 1);
  read_response(client, "+OK\r\n");
  const char get[] = "*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n";
  send_fragmented(client, get, sizeof(get) - 1);
  read_response(client, "$5\r\n");
  char value[7] = {0};
  assert(recv(client, value, sizeof(value), MSG_WAITALL) == (ssize_t)sizeof(value));
  assert(memcmp(value, "value\r\n", sizeof(value)) == 0);

  tn_redis_set_clock(UINT64_C(1000000000));
  const char set_ttl[] = "*5\r\n$3\r\nSET\r\n$3\r\nttl\r\n$1\r\nx\r\n$2\r\nEX\r\n$1\r\n1\r\n";
  send_fragmented(client, set_ttl, sizeof(set_ttl) - 1);
  read_response(client, "+OK\r\n");
  const char ttl[] = "*2\r\n$3\r\nTTL\r\n$3\r\nttl\r\n";
  send_fragmented(client, ttl, sizeof(ttl) - 1);
  read_response(client, ":1\r\n");
  tn_redis_set_clock(UINT64_C(2000000001));
  send_fragmented(client, ttl, sizeof(ttl) - 1);
  read_response(client, ":-2\r\n");
  const char get_ttl[] = "*2\r\n$3\r\nGET\r\n$3\r\nttl\r\n";
  send_fragmented(client, get_ttl, sizeof(get_ttl) - 1);
  read_response(client, "$-1\r\n");
  tn_redis_clear_clock();
  close(client);
  stop_and_join(thread);
  return 0;
}
