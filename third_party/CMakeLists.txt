add_compile_options(-Wno-everything)

################################################################################
### snappy
################################################################################
add_subdirectory(snappy)

################################################################################
### arrow - use vcpkg package
################################################################################
find_package(Arrow REQUIRED)
find_package(Parquet REQUIRED)

if(Arrow_FOUND)
    message(STATUS "Found Arrow: ${Arrow_VERSION}")
    message(STATUS "Arrow include dirs: ${Arrow_INCLUDE_DIRS}")
    message(STATUS "Arrow libraries: ${Arrow_LIBRARIES}")

    # Create alias targets to match the expected names in the main project
    if(TARGET Arrow::arrow_static)
        add_library(arrow.a ALIAS Arrow::arrow_static)
        message(STATUS "Created alias: arrow.a -> Arrow::arrow_static")
    elseif(TARGET arrow_static)
        add_library(arrow.a ALIAS arrow_static)
        message(STATUS "Created alias: arrow.a -> arrow_static")
    endif()

    if(TARGET Parquet::parquet_static)
        add_library(parquet.a ALIAS Parquet::parquet_static)
        message(STATUS "Created alias: parquet.a -> Parquet::parquet_static")
    elseif(TARGET parquet_static)
        add_library(parquet.a ALIAS parquet_static)
        message(STATUS "Created alias: parquet.a -> parquet_static")
    endif()
else()
    message(FATAL_ERROR "Arrow not found. Please install via vcpkg.")
endif()

# Build doctest
add_subdirectory(googletest)

# Build magic_enum
add_subdirectory(magic_enum)

# Build mlas
add_subdirectory(mlas)

#add_library(libhv INTERFACE)
#target_include_directories(libhv INTERFACE libhv/include)

# Build newpfor
add_subdirectory(newpfor)

# Build zsv
add_subdirectory(zsv)

################################################################################
### sse2neon
### need this after highway and before simdcomp
################################################################################

add_library(sse2neon INTERFACE)

if (ARCH_AARCH64)
  set(SSE2NEON_ROOT ${CMAKE_CURRENT_SOURCE_DIR}/sse2neon)

  target_include_directories(
    sse2neon
    SYSTEM
    INTERFACE ${SSE2NEON_ROOT}
  )
endif ()

################################################################################
### thrift
################################################################################
add_subdirectory(thrift)

################################################################################
### thrift
################################################################################
add_subdirectory(oatpp)

################################################################################
### fastpfor
################################################################################
add_subdirectory(fastpfor)

################################################################################
### ijma
################################################################################
add_subdirectory(ijma)

################################################################################
### opencc
################################################################################
add_subdirectory(opencc)

add_subdirectory(minio-cpp)
add_subdirectory(curlpp)
add_subdirectory(pugixml)

add_library(inih STATIC
        inih/cpp/INIReader.cpp
        inih/ini.c
)

add_subdirectory(curl)

add_subdirectory(re2)

add_subdirectory(pcre2)
target_compile_options(
        inih
        PRIVATE
        -O3 -Wno-sign-compare
        -fPIC
)

add_subdirectory(rocksdb)