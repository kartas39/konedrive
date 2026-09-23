cmake_minimum_required(VERSION 3.24)
project(konedrive-app VERSION 0.1.0 LANGUAGES CXX)

set(QT_MIN_VERSION "6.8.0")
set(KF_MIN_VERSION "6.8.0")

find_package(ECM ${KF_MIN_VERSION} REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings NO_POLICY_SCOPE)

find_package(Qt6 ${QT_MIN_VERSION} REQUIRED COMPONENTS Core Gui DBus Test)

set(KONEDRIVE_DBUS_XML ${CMAKE_CURRENT_SOURCE_DIR}/../dbus/org.konedrive.Account1.xml)
qt_add_dbus_interface(konedrive_dbus_SRCS ${KONEDRIVE_DBUS_XML} account1interface)

add_library(konedrive_core STATIC accountcontroller.cpp ${konedrive_dbus_SRCS})
target_include_directories(konedrive_core PUBLIC ${CMAKE_CURRENT_SOURCE_DIR} ${CMAKE_CURRENT_BINARY_DIR})
target_link_libraries(konedrive_core PUBLIC Qt6::Core Qt6::Gui Qt6::DBus)

if(BUILD_TESTING)
    add_executable(accountcontrollertest tests/accountcontrollertest.cpp)
    target_link_libraries(accountcontrollertest PRIVATE konedrive_core Qt6::Test)
    # A private session bus: the test never talks to the real daemon.
    add_test(NAME accountcontrollertest COMMAND dbus-run-session -- $<TARGET_FILE:accountcontrollertest>)
endif()

find_package(Qt6 ${QT_MIN_VERSION} REQUIRED COMPONENTS Qml Quick QuickControls2 Widgets)
find_package(KF6 ${KF_MIN_VERSION} REQUIRED COMPONENTS CoreAddons I18n Kirigami)
find_package(KF6KirigamiAddons 1.0 REQUIRED)

add_executable(konedrive main.cpp)
qt_add_resources(konedrive "konedrive_qml" PREFIX "/" FILES Main.qml)
target_link_libraries(konedrive PRIVATE
    konedrive_core
    Qt6::Qml Qt6::Quick Qt6::QuickControls2 Qt6::Widgets
    KF6::CoreAddons KF6::I18n KF6::I18nQml)
install(TARGETS konedrive ${KDE_INSTALL_TARGETS_DEFAULT_ARGS})

configure_file(org.konedrive.KOneDrive.desktop.in ${CMAKE_CURRENT_BINARY_DIR}/org.konedrive.KOneDrive.desktop @ONLY)
install(FILES ${CMAKE_CURRENT_BINARY_DIR}/org.konedrive.KOneDrive.desktop DESTINATION ${KDE_INSTALL_APPDIR})
