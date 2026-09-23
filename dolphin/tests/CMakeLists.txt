find_package(Qt6 ${QT_MIN_VERSION} REQUIRED COMPONENTS Test)

# A private session bus on which nothing can be started on demand: the tests
# never reach the user's own bus, or a daemon installed there.
configure_file(session-bus.conf.in ${CMAKE_CURRENT_BINARY_DIR}/session-bus.conf @ONLY)
# The same, except that org.konedrive.Daemon can be started on demand -- by a
# program that fails at once, as a broken install would.
set(KONEDRIVE_TEST_SERVICE_DIR ${CMAKE_CURRENT_BINARY_DIR}/activation)
configure_file(failing-daemon.service.in ${KONEDRIVE_TEST_SERVICE_DIR}/org.konedrive.Daemon.service @ONLY)
configure_file(activating-bus.conf.in ${CMAKE_CURRENT_BINARY_DIR}/activating-bus.conf @ONLY)

set(test_home ${CMAKE_CURRENT_BINARY_DIR}/home)
# The last two make glibc fill every freed block with a pattern and skip its
# per-thread cache, so a read of freed memory crashes instead of seeing the
# old bytes by luck (an eviction once read a map key it had just erased, and
# passed under glibc's defaults).
set(test_environment
    QT_QPA_PLATFORM=offscreen
    QT_PLUGIN_PATH=${CMAKE_BINARY_DIR}/bin
    LANGUAGE=en_US
    LC_ALL=C.UTF-8
    XDG_CONFIG_HOME=${test_home}/config
    XDG_DATA_HOME=${test_home}/data
    XDG_CACHE_HOME=${test_home}/cache
    XDG_STATE_HOME=${test_home}/state
    GLIBC_TUNABLES=glibc.malloc.tcache_count=0
    MALLOC_PERTURB_=165)

function(konedrive_dolphin_test name)
    add_executable(${name} ${name}.cpp testsupport.h)
    target_link_libraries(${name} PRIVATE konedrive_dolphin_core Qt6::Test Qt6::Widgets KF6::KIOWidgets KF6::KIOCore KF6::CoreAddons)
    target_compile_definitions(${name} PRIVATE
        KONEDRIVE_OVERLAY_PLUGIN="$<TARGET_FILE:konedriveoverlay>"
        KONEDRIVE_ACTIONS_PLUGIN="$<TARGET_FILE:konedriveactions>"
        KONEDRIVE_PLUGIN_DIR="${CMAKE_BINARY_DIR}/bin")
    add_dependencies(${name} konedriveoverlay konedriveactions)
    add_test(NAME ${name} COMMAND dbus-run-session --config-file=${CMAKE_CURRENT_BINARY_DIR}/session-bus.conf -- $<TARGET_FILE:${name}>)
    set_tests_properties(${name} PROPERTIES ENVIRONMENT "${test_environment}" TIMEOUT 120)
endfunction()

konedrive_dolphin_test(overlayenginetest)
konedrive_dolphin_test(overlayplugintest)
konedrive_dolphin_test(actionplugintest)
konedrive_dolphin_test(noopentest)

# Two functions of actionplugintest need a bus of their own, or half a minute.
add_test(NAME actionplugintest_activation
         COMMAND dbus-run-session --config-file=${CMAKE_CURRENT_BINARY_DIR}/activating-bus.conf -- $<TARGET_FILE:actionplugintest> daemonFailsToStart)
set_tests_properties(actionplugintest_activation PROPERTIES ENVIRONMENT "${test_environment};KONEDRIVE_TEST_ACTIVATING_BUS=1" TIMEOUT 60)
add_test(NAME actionplugintest_slow
         COMMAND dbus-run-session --config-file=${CMAKE_CURRENT_BINARY_DIR}/session-bus.conf -- $<TARGET_FILE:actionplugintest> downloadLongerThanTheDefaultTimeoutIsNotAFailure)
set_tests_properties(actionplugintest_slow PROPERTIES ENVIRONMENT "${test_environment};KONEDRIVE_TEST_SLOW=1" TIMEOUT 120)
