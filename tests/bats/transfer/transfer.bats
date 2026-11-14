#!/usr/bin/env bats

load ../util.bash

project="./tests/bats/transfer"

@test "Client is held and transferred with the transfer join method" {
    # stop the client container
    stop_container mc-client-transfer

    # stop the server container
    stop_container mc-transfer

    # restart the lazymc container
    restart_container lazymc-transfer

    # reset the start timestamp
    reset_timestamp

    #
    # The above steps ensures that the containers are in a clean state
    #

    # wait for the config to be generated with the transfer join method
    wait_for_log "lazymc-transfer" "\"transfer\", \"kick\""

    # wait for lazymc-docker-proxy to to be ready
    wait_for_formatted_log "lazymc-transfer" "INFO" "lazymc-docker-proxy::health" "Application is healthy." 300

    # start the client container
    start_container mc-client-transfer

    # wait for lazymc to start the server for the client
    wait_for_formatted_log "lazymc-transfer" "INFO" "mc::lazymc" "Starting server for 'test-bot'..." 300

    # wait for the server to be online
    wait_for_formatted_log "lazymc-transfer" "INFO" "mc::lazymc::monitor" "Server is now online" 300

    # wait for the client to be transferred to the address it connected to,
    # not to the 0.0.0.0 address lazymc listens on
    wait_for_formatted_log "lazymc-transfer" "INFO" "mc::lazymc" "Server ready, transferring client to 172.24.0.2:25565" 300

    # disconnect the client
    stop_container mc-client-transfer

    # wait for the server to be idle
    wait_for_formatted_log "lazymc-transfer" "INFO" "mc::lazymc::monitor" "Server has been idle, sleeping..." 120

    # wait for lazymc to sleep
    wait_for_formatted_log "lazymc-transfer" "INFO" "mc::lazymc::monitor" "Server is now sleeping" 120
}
