package com.example.app;

import java.util.List;
import java.util.ArrayList;

public class Greeter {
    private final String name;
    public static final String DEFAULT = "world";

    public Greeter(String name) {
        this.name = name;
    }

    public String greet() {
        return "Hello, " + name;
    }

    public <T> List<T> wrap(T item) {
        List<T> list = new ArrayList<>();
        list.add(item);
        return list;
    }

    interface Named {
        String label();
    }

    enum Color { RED, GREEN, BLUE }

    static class Inner {
        int counter;
        void bump() { counter++; }
    }
}
