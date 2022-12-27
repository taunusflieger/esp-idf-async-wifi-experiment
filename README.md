# esp-idf-async-wifi-experiment
Minimal esp-idf async wifi example

Shows how to process the ESP-IDF sysloop WifiEvents and IpEvents asynchronously using EspExecutor. On top the start and stop of the http server is made dependent on the Wifi status. Communication of events is based on embassy-sync PubSub channel.
