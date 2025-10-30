#!/bin/bash

# Helix Integration API Test Script
# This script tests all the HTTP API endpoints of the Helix integration

set -e

# Configuration
BASE_URL="http://localhost:3030"
TIMEOUT=10
VERBOSE=false

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        -u|--url)
            BASE_URL="$2"
            shift 2
            ;;
        -v|--verbose)
            VERBOSE=true
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [OPTIONS]"
            echo "Options:"
            echo "  -u, --url URL     Base URL for API (default: http://localhost:3030)"
            echo "  -v, --verbose     Verbose output"
            echo "  -h, --help        Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Helper functions
log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Test helper function
test_endpoint() {
    local method=$1
    local endpoint=$2
    local data=$3
    local expected_status=$4
    local description=$5
    
    log_info "Testing: $description"
    
    local curl_cmd="curl -s -w '%{http_code}' --connect-timeout $TIMEOUT"
    
    if [ "$method" = "POST" ] || [ "$method" = "PUT" ]; then
        curl_cmd="$curl_cmd -X $method -H 'Content-Type: application/json'"
        if [ -n "$data" ]; then
            curl_cmd="$curl_cmd -d '$data'"
        fi
    elif [ "$method" = "DELETE" ]; then
        curl_cmd="$curl_cmd -X DELETE"
    fi
    
    local url="$BASE_URL$endpoint"
    curl_cmd="$curl_cmd '$url'"
    
    if [ "$VERBOSE" = true ]; then
        echo "  Command: $curl_cmd"
    fi
    
    local response
    response=$(eval $curl_cmd 2>/dev/null)
    local status_code="${response: -3}"
    local body="${response%???}"
    
    if [ "$status_code" = "$expected_status" ]; then
        log_success "$description - Status: $status_code"
        if [ "$VERBOSE" = true ] && [ -n "$body" ]; then
            echo "  Response: $body" | head -c 200
            echo
        fi
        return 0
    else
        log_error "$description - Expected: $expected_status, Got: $status_code"
        if [ -n "$body" ]; then
            echo "  Response: $body" | head -c 200
            echo
        fi
        return 1
    fi
}

# Check if server is running
check_server() {
    log_info "Checking if Helix integration server is running..."
    
    if ! curl -s --connect-timeout 5 "$BASE_URL/health" > /dev/null 2>&1; then
        log_error "Server is not responding at $BASE_URL"
        log_error "Make sure Zed is running with Helix integration enabled"
        log_error "Expected settings in ~/.config/zed/settings.json:"
        echo '{
  "helix_integration": {
    "enabled": true,
    "server": {
      "enabled": true,
      "host": "127.0.0.1",
      "port": 3030
    }
  }
}'
        exit 1
    fi
    
    log_success "Server is responding"
}

# Main test suite
run_tests() {
    local passed=0
    local total=0
    
    echo
    log_info "Starting Helix Integration API Tests"
    echo "Testing endpoint: $BASE_URL"
    echo "==============================================="
    
    # Test 1: Health check
    ((total++))
    if test_endpoint "GET" "/health" "" "200" "Health check endpoint"; then
        ((passed++))
    fi
    
    # Test 2: Session information
    ((total++))
    if test_endpoint "GET" "/api/v1/session" "" "200" "Get session information"; then
        ((passed++))
    fi
    
    # Test 3: List contexts
    ((total++))
    if test_endpoint "GET" "/api/v1/contexts" "" "200" "List all contexts"; then
        ((passed++))
    fi
    
    # Test 4: Create context
    ((total++))
    local context_data='{"title": "Test Context", "initial_message": "Hello world"}'
    if test_endpoint "POST" "/api/v1/contexts" "$context_data" "200" "Create new context"; then
        ((passed++))
    fi
    
    # Test 5: Get context details (using placeholder ID)
    ((total++))
    if test_endpoint "GET" "/api/v1/contexts/example-context" "" "200" "Get context details"; then
        ((passed++))
    fi
    
    # Test 6: Get messages from context
    ((total++))
    if test_endpoint "GET" "/api/v1/contexts/example-context/messages" "" "200" "Get context messages"; then
        ((passed++))
    fi
    
    # Test 7: Add message to context
    ((total++))
    local message_data='{"content": "Test message from API", "role": "user"}'
    if test_endpoint "POST" "/api/v1/contexts/example-context/messages" "$message_data" "200" "Add message to context"; then
        ((passed++))
    fi
    
    # Test 8: MCP tools list
    ((total++))
    if test_endpoint "GET" "/api/v1/mcp/tools" "" "200" "List MCP tools"; then
        ((passed++))
    fi
    
    # Test 9: Sync status
    ((total++))
    if test_endpoint "GET" "/api/v1/sync/status" "" "200" "Get sync status"; then
        ((passed++))
    fi
    
    # Test 10: Sync events
    ((total++))
    if test_endpoint "GET" "/api/v1/sync/events" "" "200" "Get sync events"; then
        ((passed++))
    fi
    
    # Test 11: Delete context
    ((total++))
    if test_endpoint "DELETE" "/api/v1/contexts/example-context" "" "204" "Delete context"; then
        ((passed++))
    fi
    
    echo
    echo "==============================================="
    if [ $passed -eq $total ]; then
        log_success "All tests passed! ($passed/$total)"
        echo
        log_info "The Helix integration HTTP API is working correctly"
    else
        log_warning "Some tests failed ($passed/$total passed)"
        echo
        log_info "Check the failed endpoints and server logs for details"
    fi
    
    return $((total - passed))
}

# WebSocket test
test_websocket() {
    log_info "Testing WebSocket connection..."
    
    if command -v wscat > /dev/null 2>&1; then
        echo "Testing WebSocket with wscat (will timeout after 5 seconds)..."
        timeout 5 wscat -c "ws://localhost:3030/api/v1/ws" -x '{"type":"ping","data":{}}' || true
        log_info "WebSocket test completed (check output above)"
    elif command -v curl > /dev/null 2>&1; then
        # Try to upgrade to WebSocket (will fail but shows if endpoint exists)
        local ws_response
        ws_response=$(curl -s -I -H "Connection: Upgrade" -H "Upgrade: websocket" -H "Sec-WebSocket-Version: 13" -H "Sec-WebSocket-Key: test" "$BASE_URL/api/v1/ws" 2>/dev/null || true)
        if echo "$ws_response" | grep -q "101\|400\|426"; then
            log_success "WebSocket endpoint is accessible"
        else
            log_warning "WebSocket endpoint may not be available"
        fi
    else
        log_warning "Cannot test WebSocket (wscat or advanced curl not available)"
    fi
}

# Performance test
performance_test() {
    log_info "Running basic performance test..."
    
    local start_time=$(date +%s.%N)
    
    for i in {1..10}; do
        curl -s "$BASE_URL/health" > /dev/null 2>&1
    done
    
    local end_time=$(date +%s.%N)
    local duration=$(echo "$end_time - $start_time" | bc 2>/dev/null || echo "N/A")
    
    if [ "$duration" != "N/A" ]; then
        local avg=$(echo "scale=3; $duration / 10" | bc 2>/dev/null || echo "N/A")
        log_info "Average response time for 10 requests: ${avg}s"
    fi
}

# Main execution
main() {
    echo "Helix Integration API Test Script"
    echo "================================="
    
    check_server
    
    local exit_code=0
    
    # Run main test suite
    if ! run_tests; then
        exit_code=1
    fi
    
    # Additional tests
    echo
    test_websocket
    echo
    performance_test
    
    echo
    log_info "Test run completed"
    
    if [ $exit_code -eq 0 ]; then
        log_success "Integration is working correctly!"
        echo
        echo "Next steps:"
        echo "1. Test with a real Helix server instance"
        echo "2. Test WebSocket bidirectional communication"  
        echo "3. Test MCP tool integration"
        echo "4. Test with multiple concurrent clients"
    else
        log_error "Some tests failed. Check server logs and configuration."
    fi
    
    exit $exit_code
}

# Run main function
main "$@"