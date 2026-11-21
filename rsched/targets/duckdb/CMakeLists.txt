cmake_minimum_required(VERSION 3.20)
project(duckdb_rsched C CXX)

# DuckDB itself, configured by build.sh (no shell, no unit tests, only the
# core_functions extension, and no jemalloc so allocation goes through libc).
add_subdirectory(duckdb)
find_package(Threads REQUIRED)

add_executable(duckdb_stress stress.cpp compat.c)
target_include_directories(duckdb_stress PRIVATE duckdb/src/include)
# DuckDB's link_extension_libraries() reads extension names scoped to its own
# directory, so list the loader and the one built-in extension directly. The
# loader and duckdb_static reference each other, so the linker rescans them.
target_link_libraries(
  duckdb_stress
  "$<LINK_GROUP:RESCAN,duckdb_generated_extension_loader,core_functions_extension,duckdb_static>"
  Threads::Threads -static-libstdc++ -static-libgcc)
