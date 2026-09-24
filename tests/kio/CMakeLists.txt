# A measurement tool, not part of the product: never installed.
cmake_minimum_required(VERSION 3.24)
project(kio-probe LANGUAGES CXX)
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(ECM 6.8 REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})
find_package(Qt6 6.8 REQUIRED COMPONENTS Core Gui)
find_package(KF6 6.8 REQUIRED COMPONENTS KIO CoreAddons)

add_executable(kio_probe kio_probe.cpp)
target_link_libraries(kio_probe PRIVATE Qt6::Core Qt6::Gui KF6::KIOCore KF6::KIOGui KF6::CoreAddons)
