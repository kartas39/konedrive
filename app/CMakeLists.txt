cmake_minimum_required(VERSION 3.24)
# CMake's project version is not shown anywhere. The version the window shows
# is worked out below from Cargo.toml's (docs/releasing.md).
project(konedrive-app VERSION 0.1.0 LANGUAGES CXX)

# The version in the workspace's Cargo.toml ([workspace.package]): the next
# release's, the only place it is kept (docs/releasing.md).
set(KONEDRIVE_CARGO_TOML ${CMAKE_CURRENT_SOURCE_DIR}/../Cargo.toml)
set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS ${KONEDRIVE_CARGO_TOML})
file(READ ${KONEDRIVE_CARGO_TOML} KONEDRIVE_CARGO_TOML_TEXT)
if(NOT KONEDRIVE_CARGO_TOML_TEXT MATCHES "\\[workspace\\.package\\][^[]*\nversion = \"([^\"]+)\"")
    message(FATAL_ERROR "no version in [workspace.package] of ${KONEDRIVE_CARGO_TOML}")
endif()
set(KONEDRIVE_CARGO_VERSION ${CMAKE_MATCH_1})

# What the window shows, by the same rule as the Rust binaries
# (crates/konedrive-dbus/build.rs): the version scripts/build-rpm.sh passes
# (X.Y.Z, or X.Y.Z-dev.N), else the file's with -dev, so that a plain build
# never looks like a release; the commit it passes, else HEAD's, else unknown.
# Empty cache values mean "work it out" on every configure.
set(KONEDRIVE_BUILD_VERSION "" CACHE STRING "The version the window shows (empty: Cargo.toml's with -dev)")
set(KONEDRIVE_COMMIT "" CACHE STRING "The commit the window shows (empty: git HEAD's, else unknown)")
if(KONEDRIVE_BUILD_VERSION)
    set(KONEDRIVE_VERSION ${KONEDRIVE_BUILD_VERSION})
else()
    set(KONEDRIVE_VERSION ${KONEDRIVE_CARGO_VERSION}-dev)
endif()
if(KONEDRIVE_COMMIT)
    set(KONEDRIVE_SHOWN_COMMIT ${KONEDRIVE_COMMIT})
else()
    set(KONEDRIVE_SHOWN_COMMIT unknown)
    find_package(Git QUIET)
    if(GIT_FOUND)
        execute_process(COMMAND ${GIT_EXECUTABLE} rev-parse HEAD
            WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
            OUTPUT_VARIABLE KONEDRIVE_GIT_HEAD OUTPUT_STRIP_TRAILING_WHITESPACE
            RESULT_VARIABLE KONEDRIVE_GIT_RESULT ERROR_QUIET)
        if(KONEDRIVE_GIT_RESULT EQUAL 0 AND KONEDRIVE_GIT_HEAD MATCHES "^[0-9a-f]+$")
            set(KONEDRIVE_SHOWN_COMMIT ${KONEDRIVE_GIT_HEAD})
            # A new commit configures again: HEAD, and the branch it names.
            execute_process(COMMAND ${GIT_EXECUTABLE} symbolic-ref -q HEAD
                WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
                OUTPUT_VARIABLE branch OUTPUT_STRIP_TRAILING_WHITESPACE ERROR_QUIET)
            foreach(ref HEAD ${branch})
                execute_process(COMMAND ${GIT_EXECUTABLE} rev-parse --path-format=absolute --git-path ${ref}
                    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
                    OUTPUT_VARIABLE path OUTPUT_STRIP_TRAILING_WHITESPACE ERROR_QUIET)
                if(EXISTS "${path}")
                    set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS ${path})
                endif()
            endforeach()
        endif()
    endif()
endif()
message(STATUS "KOneDrive version: ${KONEDRIVE_VERSION} (commit ${KONEDRIVE_SHOWN_COMMIT})")

set(QT_MIN_VERSION "6.8.0")
set(KF_MIN_VERSION "6.8.0")

find_package(ECM ${KF_MIN_VERSION} REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings NO_POLICY_SCOPE)

find_package(Qt6 ${QT_MIN_VERSION} REQUIRED COMPONENTS Core Gui DBus Widgets Test)

set(KONEDRIVE_DBUS_DIR ${CMAKE_CURRENT_SOURCE_DIR}/../dbus)
qt_add_dbus_interface(konedrive_dbus_SRCS ${KONEDRIVE_DBUS_DIR}/org.konedrive.Accounts.xml accountsinterface)
qt_add_dbus_interface(konedrive_dbus_SRCS ${KONEDRIVE_DBUS_DIR}/org.konedrive.Account.xml accountinterface)
# The interfaces of an account's folder the window calls; Transfers and LocalScan
# only have properties, which SyncController reads with GetAll.
foreach(folder_interface Folder UploadQueue Conflicts ActivityLog)
    set(folder_xml ${KONEDRIVE_DBUS_DIR}/org.konedrive.${folder_interface}.xml)
    set_source_files_properties(${folder_xml} PROPERTIES INCLUDE synctypes.h)
    string(TOLOWER ${folder_interface} folder_basename)
    qt_add_dbus_interface(konedrive_dbus_SRCS ${folder_xml} ${folder_basename}interface)
endforeach()

find_package(KF6 ${KF_MIN_VERSION} REQUIRED COMPONENTS
    I18n Config Notifications StatusNotifierItem DBusAddons KIO WindowSystem JobWidgets)

add_library(konedrive_core STATIC
    daemoncontroller.cpp
    accountcontroller.cpp
    synccontroller.cpp
    accountsmodel.cpp
    currentaccount.cpp
    transfermodel.cpp
    activitymodel.cpp
    conflictmodel.cpp
    uploadreasons.cpp
    accountstatus.cpp
    appstatus.cpp
    notifier.cpp
    autostart.cpp
    trayicon.cpp
    downloadjob.cpp
    downloadjobtracker.cpp
    downloadprogresssettings.cpp
    placessettings.cpp
    placescontroller.cpp
    downloadprogresscontroller.cpp
    ${konedrive_dbus_SRCS})
target_include_directories(konedrive_core PUBLIC ${CMAKE_CURRENT_SOURCE_DIR} ${CMAKE_CURRENT_BINARY_DIR})
target_link_libraries(konedrive_core PUBLIC
    Qt6::Core Qt6::Gui Qt6::DBus Qt6::Widgets
    KF6::I18n KF6::ConfigCore KF6::Notifications KF6::StatusNotifierItem KF6::KIOGui KF6::KIOFileWidgets KF6::WindowSystem KF6::JobWidgets)
# What the autostart entry runs: the installed program, as the launcher's entry does.
target_compile_definitions(konedrive_core PRIVATE KONEDRIVE_INSTALLED_PROGRAM="${KDE_INSTALL_FULL_BINDIR}/konedrive")

find_package(Qt6 ${QT_MIN_VERSION} REQUIRED COMPONENTS Qml Quick QuickControls2)
find_package(KF6 ${KF_MIN_VERSION} REQUIRED COMPONENTS CoreAddons I18n Kirigami)
find_package(KF6KirigamiAddons 1.0 REQUIRED)

# The window's QML, a module of its own that qmlcachegen compiles into the
# binary: the window never runs a unit from Qt's disk cache, which trusts a
# resource file's unit for as long as the executable's modification time is
# unchanged, and a package's is that of its changelog day (docs/limitations-
# and-workarounds.md, R12). The C++ types stay registered imperatively in
# org.konedrive.app (qmlregistration.h), which the files import; qmlcachegen
# cannot see them, and compiles what uses them to bytecode only.
qt_policy(SET QTP0001 NEW)
qt_policy(SET QTP0004 NEW)
qt_add_library(konedrive_window STATIC)
set(KONEDRIVE_QML
    Main.qml
    qml/StatusPage.qml
    qml/ActivityPage.qml
    qml/ConflictsPage.qml
    qml/SkippedPage.qml
    qml/NotUploadedPage.qml
    qml/AccountPage.qml
    qml/SettingsPage.qml
    qml/AccountSwitcher.qml)
qt_add_qml_module(konedrive_window
    URI org.konedrive.app.window
    VERSION 1.0
    OUTPUT_DIRECTORY ${CMAKE_CURRENT_BINARY_DIR}/qml/org/konedrive/app/window
    QML_FILES ${KONEDRIVE_QML})
target_link_libraries(konedrive_window PRIVATE Qt6::Qml Qt6::Quick)

add_executable(konedrive main.cpp)
target_compile_definitions(konedrive PRIVATE
    KONEDRIVE_VERSION="${KONEDRIVE_VERSION}" KONEDRIVE_COMMIT="${KONEDRIVE_SHOWN_COMMIT}")
target_link_libraries(konedrive PRIVATE
    konedrive_core
    konedrive_windowplugin
    Qt6::Qml Qt6::Quick Qt6::QuickControls2 Qt6::Widgets
    KF6::CoreAddons KF6::I18n KF6::I18nQml KF6::DBusAddons KF6::WindowSystem)
install(TARGETS konedrive ${KDE_INSTALL_TARGETS_DEFAULT_ARGS})

configure_file(org.konedrive.KOneDrive.desktop.in ${CMAKE_CURRENT_BINARY_DIR}/org.konedrive.KOneDrive.desktop @ONLY)
install(FILES ${CMAKE_CURRENT_BINARY_DIR}/org.konedrive.KOneDrive.desktop DESTINATION ${KDE_INSTALL_APPDIR})
install(FILES konedrive.notifyrc DESTINATION ${KDE_INSTALL_KNOTIFYRCDIR})

if(BUILD_TESTING)
    # Every test runs on a private session bus with no service directories, so
    # nothing a test calls can start the real konedrived, and with the XDG
    # directories pointed into the build tree, so nothing touches ~/.config,
    # ~/.cache or ~/.local.
    set(KONEDRIVE_TEST_BUS dbus-run-session --config-file=${CMAKE_CURRENT_SOURCE_DIR}/tests/session-bus.conf --)
    set(KONEDRIVE_TEST_XDG ${CMAKE_CURRENT_BINARY_DIR}/test-xdg)
    set(KONEDRIVE_TEST_ENV
        "XDG_CONFIG_HOME=${KONEDRIVE_TEST_XDG}/config"
        "XDG_CACHE_HOME=${KONEDRIVE_TEST_XDG}/cache"
        "XDG_STATE_HOME=${KONEDRIVE_TEST_XDG}/state"
        "XDG_DATA_HOME=${KONEDRIVE_TEST_XDG}/data"
        "QT_QPA_PLATFORM=offscreen"
        "QML_DISABLE_DISK_CACHE=1")

    function(konedrive_test name)
        # fakedaemon.h holds Q_OBJECT classes: listed so AUTOMOC sees it.
        add_executable(${name} tests/${name}.cpp tests/fakedaemon.h)
        target_link_libraries(${name} PRIVATE konedrive_core Qt6::Test)
        add_test(NAME ${name} COMMAND ${KONEDRIVE_TEST_BUS} $<TARGET_FILE:${name}>)
        set_tests_properties(${name} PROPERTIES ENVIRONMENT "${KONEDRIVE_TEST_ENV}")
    endfunction()

    konedrive_test(accountcontrollertest)
    konedrive_test(accountsmodeltest)
    konedrive_test(synccontrollertest)
    konedrive_test(modelstest)
    konedrive_test(appstatustest)
    konedrive_test(notifiertest)
    konedrive_test(autostarttest)
    konedrive_test(downloadprogresssettingstest)
    konedrive_test(placescontrollertest)
    konedrive_test(downloadprogresscontrollertest)

    # Loads Main.qml as the app does, to see what its dialogs act on.
    konedrive_test(dialogstest)
    target_link_libraries(dialogstest PRIVATE konedrive_windowplugin Qt6::Qml Qt6::Quick Qt6::QuickControls2 KF6::I18nQml)

    # Loads the window with Qt's disk cache on, in an empty cache of its own:
    # the window's files leave no unit there, since they are in the binary.
    konedrive_test(qmlcachetest)
    target_link_libraries(qmlcachetest PRIVATE konedrive_windowplugin Qt6::Qml Qt6::Quick Qt6::QuickControls2 KF6::I18nQml)
    # A file outside the module, so the test sees the cache written at all.
    qt_add_resources(qmlcachetest "qmlcachetest_qml" PREFIX "/" FILES tests/CacheProbe.qml)

    # Starts the real program twice on the private bus.
    konedrive_test(singleinstancetest)
    add_dependencies(singleinstancetest konedrive)
    target_compile_definitions(singleinstancetest PRIVATE KONEDRIVE_PROGRAM="$<TARGET_FILE:konedrive>")
endif()
