#include "exports.h"

#include <stddef.h>

_Static_assert(sizeof(Pair) == sizeof(int32_t) * 2, "Pair layout changed");
_Static_assert(offsetof(Pair, left) == 0, "Pair.left layout changed");
_Static_assert(offsetof(Pair, right) == sizeof(int32_t), "Pair.right layout changed");
_Static_assert(sizeof(Kind) == sizeof(int), "C enum layout changed");

int main(void) {
  Pair pair = {.left = 19, .right = 23};
  return tn_add(19, 23) == 42 && tn_pair_value(pair) == 42 &&
                 tn_kind_value(Kind_Answer) == 42
             ? 0
             : 1;
}
