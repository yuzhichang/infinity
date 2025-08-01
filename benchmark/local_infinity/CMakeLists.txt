# infinity benchmark
add_executable(infinity_benchmark
    infinity_benchmark.cpp
)

add_dependencies(infinity_benchmark miniocpp pugixml-static curlpp_static inih libcurl_static cpptrace-lib)

target_include_directories(infinity_benchmark PUBLIC "${CMAKE_SOURCE_DIR}/src")
target_link_libraries(
    infinity_benchmark
    benchmark_profiler
    infinity_core
    sql_parser
    onnxruntime_mlas
    zsv_parser
    newpfor
    fastpfor
    #        profiler
    jma
    opencc
    dl
    parquet.a
    arrow.a
    thrift.a
    thriftnb.a
    lz4.a
    atomic.a
    event.a
    c++.a
    c++abi.a
    snappy.a
    ${JEMALLOC_STATIC_LIB}
    miniocpp.a
    re2.a
    pcre2-8-static
    pugixml-static
    curlpp_static
    inih.a
    libcurl_static
    ssl.a
    crypto.a
    rocksdb.a
    cpptrace.a
    backtrace.a
)

target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/lib")
target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/arrow/")
target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/snappy/")
target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/minio-cpp/")
target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pugixml/")
target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curlpp/")
target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curl/")
target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/re2/")
target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pcre2/")
target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/")
target_link_directories(infinity_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/cpptrace/")
target_link_directories(infinity_benchmark PUBLIC "/usr/local/openssl30/lib64")

# ########################################
# knn
# import benchmark
add_executable(knn_import_benchmark
    ./knn/knn_import_benchmark.cpp
)
add_dependencies(knn_import_benchmark miniocpp inih)

add_dependencies(knn_import_benchmark miniocpp pugixml-static curlpp_static inih libcurl_static cpptrace-lib)

target_include_directories(knn_import_benchmark PUBLIC "${CMAKE_SOURCE_DIR}/src")
target_link_libraries(
    knn_import_benchmark
    infinity_core
    benchmark_profiler
    sql_parser
    onnxruntime_mlas
    zsv_parser
    newpfor
    fastpfor
    jma
    opencc
    dl
    lz4.a
    atomic.a
    event.a
    c++.a
    c++abi.a
    #        profiler
    parquet.a
    arrow.a
    thrift.a
    thriftnb.a
    snappy.a
    ${JEMALLOC_STATIC_LIB}
    miniocpp.a
    re2.a
    pcre2-8-static
    pugixml-static
    curlpp_static
    inih.a
    libcurl_static
    ssl.a
    crypto.a
    rocksdb.a
    cpptrace.a
    backtrace.a
)

target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/lib")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/arrow/")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/snappy/")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/minio-cpp/")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pugixml/")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curlpp/")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curl/")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/re2/")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pcre2/")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/rocksdb/")
target_link_directories(knn_import_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/cpptrace/")
target_link_directories(knn_import_benchmark PUBLIC "/usr/local/openssl30/lib64")

# query benchmark
add_executable(knn_query_benchmark
    ./knn/knn_query_benchmark.cpp
)
add_dependencies(knn_query_benchmark miniocpp inih)

add_dependencies(knn_query_benchmark miniocpp pugixml-static curlpp_static inih libcurl_static cpptrace-lib)

target_include_directories(knn_query_benchmark PUBLIC "${CMAKE_SOURCE_DIR}/src")
target_link_libraries(
    knn_query_benchmark
    infinity_core
    benchmark_profiler
    sql_parser
    onnxruntime_mlas
    zsv_parser
    newpfor
    fastpfor
    jma
    opencc
    dl
    lz4.a
    atomic.a
    c++.a
    c++abi.a
    parquet.a
    arrow.a
    thrift.a
    thriftnb.a
    snappy.a
    ${JEMALLOC_STATIC_LIB}
    miniocpp.a
    re2.a
    pcre2-8-static
    pugixml-static
    curlpp_static
    inih.a
    libcurl_static
    ssl.a
    crypto.a
    rocksdb.a
    cpptrace.a
    backtrace.a
)

target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/lib")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/arrow/")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/snappy/")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/minio-cpp/")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pugixml/")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curlpp/")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curl/")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/re2/")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pcre2/")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/rocksdb/")
target_link_directories(knn_query_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/cpptrace/")
target_link_directories(knn_query_benchmark PUBLIC "/usr/local/openssl30/lib64")

# ########################################
# fulltext
# import benchmark
add_executable(fulltext_benchmark
    ./fulltext/fulltext_benchmark.cpp
)

add_dependencies(fulltext_benchmark miniocpp pugixml-static curlpp_static inih libcurl_static cpptrace-lib)

target_include_directories(fulltext_benchmark PUBLIC "${CMAKE_SOURCE_DIR}/src")
target_link_libraries(
    fulltext_benchmark
    infinity_core
    benchmark_profiler
    sql_parser
    onnxruntime_mlas
    zsv_parser
    newpfor
    fastpfor
    jma
    opencc
    dl
    lz4.a
    atomic.a
    c++.a
    c++abi.a
    parquet.a
    arrow.a
    thrift.a
    thriftnb.a
    snappy.a
    ${JEMALLOC_STATIC_LIB}
    miniocpp.a
    re2.a
    pcre2-8-static
    pugixml-static
    curlpp_static
    inih.a
    libcurl_static
    ssl.a
    crypto.a
    rocksdb.a
    cpptrace.a
    backtrace.a
)

target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/lib")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/arrow/")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/snappy/")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/minio-cpp/")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pugixml/")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curlpp/")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curl/")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/re2/")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pcre2/")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/rocksdb/")
target_link_directories(fulltext_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/cpptrace/")
target_link_directories(fulltext_benchmark PUBLIC "/usr/local/openssl30/lib64")

# ########################################
add_executable(sparse_benchmark
    ./sparse/sparse_benchmark.cpp
)

add_dependencies(sparse_benchmark miniocpp pugixml-static curlpp_static inih libcurl_static cpptrace-lib)

target_include_directories(sparse_benchmark PUBLIC "${CMAKE_SOURCE_DIR}/src")
target_link_libraries(
    sparse_benchmark
    infinity_core
    benchmark_profiler
    sql_parser
    onnxruntime_mlas
    zsv_parser
    newpfor
    fastpfor
    jma
    opencc
    dl
    lz4.a
    atomic.a
    c++.a
    c++abi.a
    parquet.a
    arrow.a
    thrift.a
    thriftnb.a
    snappy.a
    ${JEMALLOC_STATIC_LIB}
    miniocpp.a
    re2.a
    pcre2-8-static
    pugixml-static
    curlpp_static
    inih.a
    libcurl_static
    ssl.a
    crypto.a
    rocksdb.a
    cpptrace.a
    backtrace.a
)

target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/lib")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/arrow/")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/snappy/")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/minio-cpp/")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pugixml/")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curlpp/")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curl/")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/re2/")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pcre2/")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/rocksdb/")
target_link_directories(sparse_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/cpptrace/")
target_link_directories(sparse_benchmark PUBLIC "/usr/local/openssl30/lib64")

add_executable(bmp_benchmark
    ./sparse/bmp_benchmark.cpp
)

add_dependencies(bmp_benchmark miniocpp pugixml-static curlpp_static inih libcurl_static cpptrace-lib)

target_include_directories(bmp_benchmark PUBLIC "${CMAKE_SOURCE_DIR}/src")
target_link_libraries(
    bmp_benchmark
    infinity_core
    benchmark_profiler
    sql_parser
    onnxruntime_mlas
    zsv_parser
    newpfor
    fastpfor
    jma
    opencc
    dl
    lz4.a
    atomic.a
    c++.a
    c++abi.a
    parquet.a
    arrow.a
    thrift.a
    thriftnb.a
    snappy.a
    ${JEMALLOC_STATIC_LIB}
    miniocpp.a
    re2.a
    pcre2-8-static
    pugixml-static
    curlpp_static
    inih.a
    libcurl_static
    ssl.a
    crypto.a
    rocksdb.a
    cpptrace.a
    backtrace.a
)

target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/lib")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/arrow/")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/snappy/")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/minio-cpp/")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pugixml/")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curlpp/")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curl/")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/re2/")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pcre2/")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/rocksdb/")
target_link_directories(bmp_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/cpptrace/")
target_link_directories(bmp_benchmark PUBLIC "/usr/local/openssl30/lib64")

add_executable(hnsw_benchmark
    ./knn/hnsw_benchmark.cpp
)

add_dependencies(hnsw_benchmark miniocpp pugixml-static curlpp_static inih libcurl_static cpptrace-lib)

target_include_directories(hnsw_benchmark PUBLIC "${CMAKE_SOURCE_DIR}/src")
target_link_libraries(
    hnsw_benchmark
    infinity_core
    benchmark_profiler
    sql_parser
    onnxruntime_mlas
    zsv_parser
    newpfor
    fastpfor
    jma
    opencc
    dl
    lz4.a
    atomic.a
    c++.a
    c++abi.a
    parquet.a
    arrow.a
    thrift.a
    thriftnb.a
    snappy.a
    ${JEMALLOC_STATIC_LIB}
    miniocpp.a
    re2.a
    pcre2-8-static
    pugixml-static
    curlpp_static
    inih.a
    libcurl_static
    ssl.a
    crypto.a
    rocksdb.a
    cpptrace.a
    backtrace.a
)

target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/lib")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/arrow/")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/snappy/")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/minio-cpp/")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pugixml/")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curlpp/")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/curl/")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/re2/")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/pcre2/")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/rocksdb/")
target_link_directories(hnsw_benchmark PUBLIC "${CMAKE_BINARY_DIR}/third_party/cpptrace/")
target_link_directories(hnsw_benchmark PUBLIC "/usr/local/openssl30/lib64")

# add_definitions(-march=native)
# add_definitions(-msse4.2 -mfma)
# add_definitions(-mavx2 -mf16c -mpopcnt)

# execute_process(COMMAND grep -q avx2 /proc/cpuinfo  
#                 RESULT_VARIABLE SUPPORT_AVX2  
#                 OUTPUT_QUIET  
#                 ERROR_QUIET)  

# execute_process(COMMAND grep -q avx512 /proc/cpuinfo  
# RESULT_VARIABLE SUPPORT_AVX512  
# OUTPUT_QUIET  
# ERROR_QUIET)

# if (SUPPORT_AVX2 EQUAL 0 OR SUPPORT_AVX512 EQUAL 0)
#         message("Compiled by AVX2 or AVX512")
#         target_compile_options(infinity_benchmark PUBLIC $<$<COMPILE_LANGUAGE:CXX>:-march=native>)
#         target_compile_options(knn_import_benchmark PUBLIC $<$<COMPILE_LANGUAGE:CXX>:-march=native>)
#         target_compile_options(knn_query_benchmark PUBLIC $<$<COMPILE_LANGUAGE:CXX>:-march=native)
# else()
#         message("Compiled by SSE")
#         target_compile_options(infinity_benchmark PUBLIC $<$<COMPILE_LANGUAGE:CXX>:-msse4.2 -mfma>)
#         target_compile_options(knn_import_benchmark PUBLIC $<$<COMPILE_LANGUAGE:CXX>:-msse4.2 -mfma>)
#         target_compile_options(knn_query_benchmark PUBLIC $<$<COMPILE_LANGUAGE:CXX>:-msse4.2 -mfma>)
# endif()
