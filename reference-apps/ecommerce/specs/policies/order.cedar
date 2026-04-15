// Cedar ABAC Policies for Order Entity
// Temper Reference E-Commerce App
//
// Principal types: Customer, Agent (CustomerAgent, OperationsAgent, SupportAgent), Admin
// Resource types: Order, OrderItem, Payment, Shipment
// Action types: read, create, update, delete, submitOrder, cancelOrder, confirmOrder,
//               initiateReturn, processOrder, shipOrder, refundPayment

// ============================================================
// Customer Policies
// ============================================================

// Customers can read their own orders
permit(
    principal is Customer,
    action == Action::"read",
    resource is Order
) when {
    resource.customerId == principal.id
};

// Customers can create draft orders (via agent)
permit(
    principal is Customer,
    action == Action::"create",
    resource is Order
) when {
    resource.status == "Draft"
};

// Customers can modify their own draft orders
permit(
    principal is Customer,
    action == Action::"update",
    resource is Order
) when {
    resource.customerId == principal.id &&
    resource.status == "Draft"
};

// Customers can submit their own draft orders
permit(
    principal is Customer,
    action == Action::"submitOrder",
    resource is Order
) when {
    resource.customerId == principal.id &&
    resource.status == "Draft"
};

// Customers can cancel their own orders (if in cancellable state)
permit(
    principal is Customer,
    action == Action::"cancelOrder",
    resource is Order
) when {
    resource.customerId == principal.id &&
    (resource.status == "Draft" ||
     resource.status == "Submitted" ||
     resource.status == "Confirmed")
};

// Customers can initiate returns on their own shipped/delivered orders
permit(
    principal is Customer,
    action == Action::"initiateReturn",
    resource is Order
) when {
    resource.customerId == principal.id &&
    (resource.status == "Shipped" || resource.status == "Delivered")
};

// ============================================================
// Agent Policies (LLM agents acting on behalf of customers)
// ============================================================

// Customer-facing agents inherit customer's permissions
// (agent.customerId is set when agent authenticates on behalf of customer)
permit(
    principal is Agent,
    action == Action::"read",
    resource is Order
) when {
    principal.role == "customer_agent" &&
    resource.customerId == principal.actingFor
};

permit(
    principal is Agent,
    action in [Action::"create", Action::"update", Action::"submitOrder",
               Action::"cancelOrder", Action::"initiateReturn"],
    resource is Order
) when {
    principal.role == "customer_agent" &&
    resource.customerId == principal.actingFor
};

// ============================================================
// Operations Agent Policies
// ============================================================

// Operations agents can read all orders
permit(
    principal is Agent,
    action == Action::"read",
    resource is Order
) when {
    principal.role == "operations_agent"
};

// Operations agents can confirm, process, and ship orders
permit(
    principal is Agent,
    action in [Action::"confirmOrder", Action::"processOrder", Action::"shipOrder"],
    resource is Order
) when {
    principal.role == "operations_agent"
};

// ============================================================
// Support Agent Policies
// ============================================================

// Support agents can read all orders
permit(
    principal is Agent,
    action == Action::"read",
    resource is Order
) when {
    principal.role == "support_agent"
};

// Support agents can cancel orders and initiate returns (customer service)
permit(
    principal is Agent,
    action in [Action::"cancelOrder", Action::"initiateReturn"],
    resource is Order
) when {
    principal.role == "support_agent"
};

// Support agents can process refunds
permit(
    principal is Agent,
    action == Action::"refundPayment",
    resource is Payment
) when {
    principal.role == "support_agent"
};

// ============================================================
// Admin Policies
// ============================================================

// Admins can do everything
permit(
    principal is Admin,
    action,
    resource
);

// ============================================================
// Deny Policies (override permits)
// ============================================================

// Nobody can modify cancelled orders
forbid(
    principal,
    action in [Action::"update", Action::"submitOrder"],
    resource is Order
) when {
    resource.status == "Cancelled"
};

// Nobody can modify refunded orders
forbid(
    principal,
    action in [Action::"update", Action::"cancelOrder", Action::"initiateReturn"],
    resource is Order
) when {
    resource.status == "Refunded"
};

// Rate limit: agents cannot perform more than 100 actions per minute
// (enforced via context attribute, not Cedar directly — but Cedar checks the flag)
forbid(
    principal is Agent,
    action,
    resource
) when {
    context.rateLimitExceeded == true
};
