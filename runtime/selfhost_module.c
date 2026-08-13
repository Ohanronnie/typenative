/*
 * The source-level backend stores fourteen machine words per function.  This
 * file is included by runtime.c after the LLVM C API and product helpers are
 * defined, keeping the public runtime surface in one translation unit while
 * making this larger module emitter independently reviewable.
 */

static size_t tn_selfhost_backend_word(const uint8_t *functions, size_t index) {
  size_t value = 0;
  memcpy(&value, functions + index * sizeof(size_t), sizeof(value));
  return value;
}

static void tn_selfhost_free_backend_names(char **names, size_t count) {
  for (size_t index = 0; index < count; ++index) {
    free(names[index]);
  }
}

static int32_t tn_selfhost_llvm_emit_i32_module_product_impl(const char *output_path, const char *module_name,
                                                             const uint8_t *source, size_t source_length,
                                                             const uint8_t *functions, size_t function_count,
                                                             const uint8_t *operations, size_t operation_count,
                                                             const char *entry_name, int32_t entry_returns_void,
                                                             int32_t product) {
  if (output_path == NULL || module_name == NULL || entry_name == NULL ||
      (source == NULL && source_length != 0) || (functions == NULL && function_count != 0) ||
      (operations == NULL && operation_count != 0) || function_count == 0 || function_count > 256 ||
      operation_count == 0 || operation_count > 16384 || (entry_returns_void != 0 && entry_returns_void != 1)) {
    return -EINVAL;
  }
  if (function_count > SIZE_MAX / 14 || function_count * 14 > SIZE_MAX / sizeof(size_t)) {
    return -EOVERFLOW;
  }

  pthread_once(&tn_selfhost_llvm_api_once, tn_selfhost_llvm_load_api_once);
  tn_selfhost_llvm_api *api = &tn_selfhost_llvm_api_state;
  if (api->context_create == NULL || api->module_create == NULL || api->module_dispose == NULL ||
      api->context_dispose == NULL || api->int32_type == NULL || api->void_type == NULL ||
      api->function_type == NULL || api->add_function == NULL || api->get_param == NULL ||
      api->append_basic_block == NULL || api->create_builder == NULL || api->position_builder == NULL ||
      api->const_int == NULL || api->build_add == NULL || api->build_sub == NULL ||
      api->build_mul == NULL || api->build_sdiv == NULL || api->build_srem == NULL ||
      api->build_neg == NULL || api->build_icmp == NULL || api->build_select == NULL || api->build_zext == NULL ||
      api->build_call2 == NULL || api->build_ret == NULL ||
      api->build_ret_void == NULL || api->dispose_builder == NULL || api->verify_module == NULL ||
      api->print_module == NULL || api->dispose_message == NULL) {
    return -ENOSYS;
  }

  size_t operation_offset[256] = {0};
  size_t parameter_count[256] = {0};
  size_t result_kind[256] = {0};
  size_t operation_sizes[256] = {0};
  char *names[256] = {0};
  void *function_values[256] = {0};
  size_t consumed_operations = 0;
  size_t main_index = SIZE_MAX;
  int32_t status = 0;

  for (size_t index = 0; index < function_count; ++index) {
    const size_t base = index * 14;
    const size_t name_start = tn_selfhost_backend_word(functions, base);
    const size_t name_end = tn_selfhost_backend_word(functions, base + 1);
    const size_t body_start = tn_selfhost_backend_word(functions, base + 2);
    const size_t body_end = tn_selfhost_backend_word(functions, base + 3);
    parameter_count[index] = tn_selfhost_backend_word(functions, base + 4);
    result_kind[index] = tn_selfhost_backend_word(functions, base + 7);
    const size_t lowering_valid = tn_selfhost_backend_word(functions, base + 11);
    operation_sizes[index] = tn_selfhost_backend_word(functions, base + 12);
    if (name_start >= name_end || name_end > source_length || body_start >= body_end || body_end > source_length ||
        parameter_count[index] > 32 || (result_kind[index] != 1 && result_kind[index] != 2) ||
        lowering_valid == 0 || operation_sizes[index] > 512 ||
        consumed_operations > operation_count - operation_sizes[index] || name_end - name_start > 255) {
      status = -EINVAL;
      break;
    }
    const size_t source_name_length = name_end - name_start;
    const char *source_name = (const char *)(source + name_start);
    const int rename_main = product == 4 && source_name_length == 4 && memcmp(source_name, "main", 4) == 0;
    const char *emitted_name = rename_main ? "tn_selfhost_main" : source_name;
    const size_t emitted_name_length = rename_main ? strlen("tn_selfhost_main") : source_name_length;
    names[index] = malloc(emitted_name_length + 1);
    if (names[index] == NULL) {
      status = -ENOMEM;
      break;
    }
    memcpy(names[index], emitted_name, emitted_name_length);
    names[index][emitted_name_length] = '\0';
    for (size_t previous = 0; previous < index; ++previous) {
      if (strcmp(names[previous], names[index]) == 0) {
        status = -EEXIST;
        break;
      }
    }
    if (status != 0) {
      break;
    }
    if (source_name_length == 4 && memcmp(source_name, "main", 4) == 0) {
      main_index = index;
    }
    operation_offset[index] = consumed_operations;
    consumed_operations += operation_sizes[index];
  }
  if (status == 0 && (consumed_operations != operation_count || main_index == SIZE_MAX)) {
    status = -EINVAL;
  }
  if (status != 0) {
    tn_selfhost_free_backend_names(names, function_count);
    return status;
  }

  void *context = api->context_create();
  if (context == NULL) {
    tn_selfhost_free_backend_names(names, function_count);
    return -ENOMEM;
  }
  void *module = api->module_create(module_name, context);
  if (module == NULL) {
    api->context_dispose(context);
    tn_selfhost_free_backend_names(names, function_count);
    return -ENOMEM;
  }
  void *integer = api->int32_type(context);
  void *void_type = api->void_type(context);
  if (integer == NULL || void_type == NULL) {
    api->module_dispose(module);
    api->context_dispose(context);
    tn_selfhost_free_backend_names(names, function_count);
    return -ENOMEM;
  }

  void *parameter_types[32];
  for (size_t index = 0; index < function_count && status == 0; ++index) {
    for (size_t parameter = 0; parameter < parameter_count[index]; ++parameter) {
      parameter_types[parameter] = integer;
    }
    void *return_type = result_kind[index] == 1 ? void_type : integer;
    void *type = api->function_type(return_type,
                                    parameter_count[index] == 0 ? NULL : parameter_types,
                                    (unsigned)parameter_count[index], 0);
    function_values[index] = type == NULL ? NULL : api->add_function(module, names[index], type);
    if (function_values[index] == NULL) {
      status = -ENOMEM;
    }
  }
  void *zero_function_type = api->function_type(integer, NULL, 0, 0);
  void *argument_count_function = NULL;
  if (status == 0) {
    argument_count_function = api->add_function(module, "tn_process_argc", zero_function_type);
    if (argument_count_function == NULL) {
      status = -ENOMEM;
    }
  }

  for (size_t index = 0; index < function_count && status == 0; ++index) {
    void *block = api->append_basic_block(context, function_values[index], "entry");
    void *builder = block == NULL ? NULL : api->create_builder(context);
    if (builder == NULL) {
      status = -ENOMEM;
      break;
    }
    api->position_builder(builder, block);
    void *parameters[32];
    for (size_t parameter = 0; parameter < parameter_count[index]; ++parameter) {
      parameters[parameter] = api->get_param(function_values[index], (unsigned)parameter);
      if (parameters[parameter] == NULL) {
        status = -EINVAL;
        break;
      }
    }
    void *values[512];
    int32_t known_values[512];
    uint8_t known[512];
    size_t depth = 0;
    const uint8_t *function_operations = operations + operation_offset[index] * sizeof(tn_selfhost_i32_operation);
    for (size_t operation_index = 0; operation_index < operation_sizes[index] && status == 0; ++operation_index) {
      tn_selfhost_i32_operation operation;
      memcpy(&operation, function_operations + operation_index * sizeof(operation), sizeof(operation));
      if (operation.kind == 0) {
        if (depth >= 512) {
          status = -E2BIG;
          break;
        }
        values[depth] = api->const_int(integer, (unsigned long long)(uint32_t)operation.value, 1);
        known[depth] = 1;
        known_values[depth] = operation.value;
        depth += 1;
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
        }
        continue;
      }
      if (operation.kind == 7) {
        if (depth >= 512) {
          status = -E2BIG;
          break;
        }
        void *argument_count_value = api->build_call2(builder, zero_function_type, argument_count_function, NULL, 0,
                                                       "argument_count");
        if (argument_count_value == NULL) {
          status = -EINVAL;
          break;
        }
        values[depth] = argument_count_value;
        known[depth] = 0;
        known_values[depth] = 0;
        depth += 1;
        continue;
      }
      if (operation.kind == 11) {
        if (depth < 3) {
          status = -EINVAL;
          break;
        }
        void *else_value = values[--depth];
        const uint8_t else_known = known[depth];
        const int32_t else_constant = known_values[depth];
        void *then_value = values[--depth];
        const uint8_t then_known = known[depth];
        const int32_t then_constant = known_values[depth];
        void *condition = values[--depth];
        const uint8_t condition_known = known[depth];
        const int32_t condition_constant = known_values[depth];
        void *zero = api->const_int(integer, 0, 1);
        void *predicate = zero == NULL ? NULL : api->build_icmp(builder, 33, condition, zero, "condition");
        void *selected = predicate == NULL ? NULL : api->build_select(builder, predicate, then_value, else_value, "select");
        if (selected == NULL || depth >= 512) {
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
        void *right = values[--depth];
        const uint8_t right_known = known[depth];
        const int32_t right_constant = known_values[depth];
        void *left = values[--depth];
        const uint8_t left_known = known[depth];
        const int32_t left_constant = known_values[depth];
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
        if (result == NULL || depth >= 512) {
          status = -EINVAL;
          break;
        }
        values[depth] = result;
        known[depth] = 0;
        known_values[depth] = 0;
        if (left_known != 0 && right_known != 0) {
          int32_t result_value = 0;
          if (operation.kind == 12) {
            result_value = left_constant == right_constant;
          } else if (operation.kind == 13) {
            result_value = left_constant != right_constant;
          } else if (operation.kind == 14) {
            result_value = left_constant < right_constant;
          } else if (operation.kind == 15) {
            result_value = left_constant <= right_constant;
          } else if (operation.kind == 16) {
            result_value = left_constant > right_constant;
          } else {
            result_value = left_constant >= right_constant;
          }
          known[depth] = 1;
          known_values[depth] = result_value;
        }
        depth += 1;
        continue;
      }
      if (operation.kind == 8) {
        if (operation.value < 0 || (size_t)operation.value >= parameter_count[index] || depth >= 512) {
          status = -EINVAL;
          break;
        }
        values[depth] = parameters[operation.value];
        known[depth] = 0;
        known_values[depth] = 0;
        depth += 1;
        continue;
      }
      if (operation.kind == 9) {
        if (operation.value < 0 || (size_t)operation.value >= function_count) {
          status = -EINVAL;
          break;
        }
        const size_t target = (size_t)operation.value;
        const size_t argument_count = parameter_count[target];
        if (result_kind[target] != 2 || depth < argument_count) {
          status = -EINVAL;
          break;
        }
        void *arguments[32];
        for (size_t argument = 0; argument < argument_count; ++argument) {
          arguments[argument] = values[depth - argument_count + argument];
        }
        depth -= argument_count;
        void *target_type = api->function_type(integer,
                                               argument_count == 0 ? NULL : parameter_types,
                                               (unsigned)argument_count, 0);
        void *called = api->build_call2(builder, target_type, function_values[target], arguments,
                                        (unsigned)argument_count, "call");
        if (called == NULL || depth >= 512) {
          status = -EINVAL;
          break;
        }
        values[depth] = called;
        known[depth] = 0;
        known_values[depth] = 0;
        depth += 1;
        continue;
      }
      if (operation.kind == 10) {
        if (operation.value < 0 || (size_t)operation.value >= function_count) {
          status = -EINVAL;
          break;
        }
        const size_t target = (size_t)operation.value;
        const size_t argument_count = parameter_count[target];
        if (result_kind[target] != 1 || depth < argument_count) {
          status = -EINVAL;
          break;
        }
        void *arguments[32];
        for (size_t argument = 0; argument < argument_count; ++argument) {
          arguments[argument] = values[depth - argument_count + argument];
        }
        depth -= argument_count;
        void *target_type = api->function_type(void_type,
                                               argument_count == 0 ? NULL : parameter_types,
                                               (unsigned)argument_count, 0);
        if (api->build_call2(builder, target_type, function_values[target], arguments,
                             (unsigned)argument_count, "") == NULL) {
          status = -EINVAL;
        }
        continue;
      }
      if (operation.kind < 2 || operation.kind > 6 || depth < 2) {
        status = -EINVAL;
        break;
      }
      void *right = values[depth - 1];
      const uint8_t right_known = known[depth - 1];
      const int32_t right_value = known_values[depth - 1];
      depth -= 1;
      void *left = values[depth - 1];
      const uint8_t left_known = known[depth - 1];
      const int32_t left_value = known_values[depth - 1];
      depth -= 1;
      void *result = NULL;
      int32_t result_value = 0;
      uint8_t result_is_known = 0;
      if (left_known != 0 && right_known != 0) {
        int64_t wide = 0;
        if (operation.kind == 2) {
          wide = (int64_t)left_value + right_value;
        } else if (operation.kind == 3) {
          wide = (int64_t)left_value - right_value;
        } else if (operation.kind == 4) {
          wide = (int64_t)left_value * right_value;
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
        result_is_known = 1;
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
      if (result == NULL || depth >= 512) {
        status = -EINVAL;
        break;
      }
      values[depth] = result;
      known[depth] = result_is_known;
      known_values[depth] = result_value;
      depth += 1;
    }
    if (status == 0) {
      if (result_kind[index] == 1) {
        if (depth != 0 || api->build_ret_void(builder) == NULL) {
          status = -EINVAL;
        }
      } else if (depth != 1 || api->build_ret(builder, values[0]) == NULL) {
        status = -EINVAL;
      }
    }
    api->dispose_builder(builder);
  }
  if (status == 0 && strcmp(entry_name, "tn_selfhost_entry") == 0) {
    if (parameter_count[main_index] != 0 || (result_kind[main_index] == 1) != (entry_returns_void != 0)) {
      status = -EINVAL;
    } else {
      void *entry_type = api->function_type(entry_returns_void != 0 ? void_type : integer, NULL, 0, 0);
      void *entry = entry_type == NULL ? NULL : api->add_function(module, entry_name, entry_type);
      void *block = entry == NULL ? NULL : api->append_basic_block(context, entry, "entry");
      void *builder = block == NULL ? NULL : api->create_builder(context);
      if (builder == NULL) {
        status = -ENOMEM;
      } else {
        api->position_builder(builder, block);
        void *main_type = api->function_type(result_kind[main_index] == 1 ? void_type : integer, NULL, 0, 0);
        void *called = api->build_call2(builder, main_type, function_values[main_index], NULL, 0,
                                        entry_returns_void != 0 ? "" : "main");
        if (called == NULL) {
          status = -EINVAL;
        } else if (entry_returns_void != 0) {
          if (api->build_ret_void(builder) == NULL) {
            status = -EINVAL;
          }
        } else if (api->build_ret(builder, called) == NULL) {
          status = -EINVAL;
        }
        api->dispose_builder(builder);
      }
    }
  }
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
    status = tn_selfhost_llvm_write_product(api, module, output_path, product, entry_returns_void);
  }
  api->module_dispose(module);
  api->context_dispose(context);
  tn_selfhost_free_backend_names(names, function_count);
  return status;
}

int32_t tn_selfhost_llvm_emit_i32_module(const char *output_path, const char *module_name,
                                         const uint8_t *source, size_t source_length,
                                         const uint8_t *functions, size_t function_count,
                                         const uint8_t *operations, size_t operation_count,
                                         const char *entry_name, int32_t entry_returns_void) {
  return tn_selfhost_llvm_emit_i32_module_product_impl(output_path, module_name, source, source_length,
                                                       functions, function_count, operations, operation_count,
                                                       entry_name, entry_returns_void, 0);
}

int32_t tn_selfhost_llvm_emit_i32_module_product(const char *output_path, const char *module_name,
                                                const uint8_t *source, size_t source_length,
                                                const uint8_t *functions, size_t function_count,
                                                const uint8_t *operations, size_t operation_count,
                                                const char *entry_name, int32_t entry_returns_void,
                                                int32_t product) {
  return tn_selfhost_llvm_emit_i32_module_product_impl(output_path, module_name, source, source_length,
                                                       functions, function_count, operations, operation_count,
                                                       entry_name, entry_returns_void, product);
}
