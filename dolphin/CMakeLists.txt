cmake_minimum_required(VERSION 3.24)
project(konedrive-dolphin VERSION 0.1.0 LANGUAGES CXX)

# Dolphin integration: an overlay emblem for each file's state and pin, and
# "Always keep on this device" / "Free up space" in the context menu. Neither
# plugin ever opens a file in the sync folder -- opening a placeholder
# downloads it.

set(QT_MIN_VERSION "6.8.0")
set(KF_MIN_VERSION "6.8.0")

find_package(ECM ${KF_MIN_VERSION} REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings NO_POLICY_SCOPE)

find_package(Qt6 ${QT_MIN_VERSION} REQUIRED COMPONENTS Core Gui Widgets DBus)
find_package(KF6 ${KF_MIN_VERSION} REQUIRED COMPONENTS KIO CoreAddons I18n)

add_definitions(-DTRANSLATION_DOMAIN=\"konedrive_dolphin\")

# Everything the two plugins decide, in one library the tests link directly.
add_library(konedrive_dolphin_core STATIC
    src/filestate.cpp
    src/overlayengine.cpp
    src/refusaltext.cpp
    src/syncclient.cpp)
set_target_properties(konedrive_dolphin_core PROPERTIES POSITION_INDEPENDENT_CODE ON)
target_include_directories(konedrive_dolphin_core PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/src)
target_link_libraries(konedrive_dolphin_core PUBLIC Qt6::Core Qt6::DBus KF6::I18n)

kcoreaddons_add_plugin(konedriveoverlay SOURCES src/overlayplugin.cpp INSTALL_NAMESPACE "kf6/overlayicon")
target_link_libraries(konedriveoverlay PRIVATE konedrive_dolphin_core KF6::KIOCore)

kcoreaddons_add_plugin(konedriveactions SOURCES src/actionplugin.cpp INSTALL_NAMESPACE "kf6/kfileitemaction")
target_link_libraries(konedriveactions PRIVATE konedrive_dolphin_core KF6::KIOWidgets KF6::CoreAddons KF6::I18n Qt6::Widgets)

if(BUILD_TESTING)
    add_subdirectory(tests)
endif()
