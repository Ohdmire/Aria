cmake_minimum_required(VERSION 3.21)
project(aria_wallpaper_plugin LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)

find_package(Qt6 REQUIRED COMPONENTS Quick Qml Gui)
find_package(PkgConfig REQUIRED)
pkg_check_modules(EGL REQUIRED egl)
pkg_check_modules(GLES REQUIRED glesv2)

add_library(ariaplugin SHARED
    ariasurface.cpp
    ariasurface.h
    plugin.cpp
)
target_include_directories(ariaplugin PRIVATE ${EGL_INCLUDE_DIRS} ${GLES_INCLUDE_DIRS})
target_link_libraries(ariaplugin PRIVATE Qt6::Quick Qt6::Qml Qt6::Gui ${EGL_LIBRARIES} ${GLES_LIBRARIES})
# 直接产出到壁纸包的 qmldir 旁边(`plugin ariaplugin` → libariaplugin.so)
set_target_properties(ariaplugin PROPERTIES
    LIBRARY_OUTPUT_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/../wallpaper/contents/ui/aria"
)
