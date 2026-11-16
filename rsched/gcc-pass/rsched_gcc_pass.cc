#include "gcc-plugin.h"
#include "plugin-version.h"

#include "context.h"
#include "diagnostic-core.h"
#include "function.h"
#include "tree.h"
#include "gimple.h"
#include "gimple-iterator.h"
#include "tree-ssa.h"
#include "tree-pass.h"

#include <cstring>

int plugin_is_GPL_compatible;

namespace {

const pass_data rsched_pass_data = {
    GIMPLE_PASS,
    "rsched_gcc_pass",
    OPTGROUP_NONE,
    TV_NONE,
    PROP_gimple_any,
    0,
    0,
    0,
    0,
};

const char *replacement_for(const char *name) {
    if (std::strcmp(name, "__clone") == 0) {
        return "rsched_clone";
    }
    if (std::strcmp(name, "__clone_internal") == 0) {
        return "rsched_clone_internal";
    }
    return nullptr;
}

tree replacement_decl(const char *name, tree original) {
    tree decl = build_fn_decl(name, TREE_TYPE(original));
    TREE_PUBLIC(decl) = 1;
    DECL_EXTERNAL(decl) = 1;
    return decl;
}

struct rsched_pass final : gimple_opt_pass {
    explicit rsched_pass(gcc::context *ctxt) : gimple_opt_pass(rsched_pass_data, ctxt) {}

    unsigned int execute(function *fun) override {
        bool changed = false;
        basic_block bb;
        FOR_EACH_BB_FN(bb, fun) {
            for (gimple_stmt_iterator gsi = gsi_start_bb(bb); !gsi_end_p(gsi);
                 gsi_next(&gsi)) {
                gimple *stmt = gsi_stmt(gsi);
                if (!is_gimple_call(stmt)) {
                    continue;
                }
                gcall *call = as_a<gcall *>(stmt);
                tree callee = gimple_call_fndecl(call);
                if (callee == nullptr || DECL_NAME(callee) == nullptr) {
                    continue;
                }
                const char *name = IDENTIFIER_POINTER(DECL_NAME(callee));
                const char *replacement = replacement_for(name);
                if (replacement == nullptr) {
                    continue;
                }
                gimple_call_set_fndecl(call, replacement_decl(replacement, callee));
                changed = true;
            }
        }
        return changed ? TODO_update_ssa : 0;
    }
};

} // namespace

int plugin_init(plugin_name_args *plugin_info, plugin_gcc_version *version) {
    if (!plugin_default_version_check(version, &gcc_version)) {
        error("rsched GCC pass was built for a different GCC version");
        return 1;
    }

    register_pass_info pass_info;
    pass_info.pass = new rsched_pass(g);
    pass_info.reference_pass_name = "cfg";
    pass_info.ref_pass_instance_number = 1;
    pass_info.pos_op = PASS_POS_INSERT_AFTER;
    register_callback(plugin_info->base_name, PLUGIN_PASS_MANAGER_SETUP, nullptr, &pass_info);
    return 0;
}
