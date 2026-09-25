cmake_minimum_required(VERSION 3.24)
project(konedrive-app VERSION 0.1.0 LANGUAGES CXX)

set(QT_MIN_VERSION "6.8.0")
set(KF_MIN_VERSION "6.8.0")

find_package(ECM ${KF_MIN_VERSION} REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings NO_POLICY_SCOPE)

find_package(Qt6 ${QT_MIN_VERSION} REQUIRED COMPONENTS Core Gui DBus Widgets Test)

set(KONEDRIVE_ACCOUNTS_XML ${CMAKE_CURRENT_SOURCE_DIR}/../dbus/org.konedrive.Accounts1.xml)
qt_add_dbus_interface(konedrive_dbus_SRCS ${KONEDRIVE_ACCOUNTS_XML} accounts1interface)

set(KONEDRIVE_DBUS_XML ${CMAKE_CURRENT_SOURCE_DIR}/../dbus/org.konedrive.Account1.xml)
qt_add_dbus_interface(konedrive_dbus_SRCS ${KONEDRIVE_DBUS_XML} account1interface)

set(KONEDRIVE_SYNC_XML ${CMAKE_CURRENT_SOURCE_DIR}/../dbus/org.konedrive.Sync1.xml)
set_source_files_properties(${KONEDRIVE_SYNC_XML} PROPERTIES INCLUDE synctypes.h)
qt_add_dbus_interface(konedrive_dbus_SRCS ${KONEDRIVE_SYNC_XML} sync1interface)

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
    outboxmodel.cpp
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

add_executable(konedrive main.cpp)
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
qt_add_resources(konedrive "konedrive_qml" PREFIX "/" FILES ${KONEDRIVE_QML})
target_link_libraries(konedrive PRIVATE
    konedrive_core
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
    target_link_libraries(dialogstest PRIVATE Qt6::Qml Qt6::Quick Qt6::QuickControls2 KF6::I18nQml)
    qt_add_resources(dialogstest "dialogstest_qml" PREFIX "/" FILES ${KONEDRIVE_QML})

    # Starts the real program twice on the private bus.
    konedrive_test(singleinstancetest)
    add_dependencies(singleinstancetest konedrive)
    target_compile_definitions(singleinstancetest PRIVATE KONEDRIVE_PROGRAM="$<TARGET_FILE:konedrive>")
endif()
